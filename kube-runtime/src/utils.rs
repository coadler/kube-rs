use crate::watcher;
use futures::{
    pin_mut,
    stream::{self, Peekable},
    Future, Stream, StreamExt, TryStream, TryStreamExt,
};
use pin_cell::{PinCell, PinMut};
use pin_project::pin_project;
use std::{fmt::Debug, pin::Pin, rc::Rc, task::Poll};
use stream::IntoStream;

/// Flattens each item in the list following the rules of `watcher::Event::into_iter_applied`
pub fn try_flatten_applied<K, S: TryStream<Ok = watcher::Event<K>>>(
    stream: S,
) -> impl Stream<Item = Result<K, S::Error>> {
    stream
        .map_ok(|event| stream::iter(event.into_iter_applied().map(Ok)))
        .try_flatten()
}

/// Flattens each item in the list following the rules of `watcher::Event::into_iter_touched`
pub fn try_flatten_touched<K, S: TryStream<Ok = watcher::Event<K>>>(
    stream: S,
) -> impl Stream<Item = Result<K, S::Error>> {
    stream
        .map_ok(|event| stream::iter(event.into_iter_touched().map(Ok)))
        .try_flatten()
}

/// Allows splitting a `Stream` into several streams that each emit a disjoint subset of the input stream's items,
/// like a streaming variant of pattern matching.
///
/// NOTE: The cases MUST be reunited into the same final stream (using `futures::stream::select` or similar),
/// since cases for rejected items will *not* register wakeup correctly, and may otherwise lose items and/or deadlock.
///
/// NOTE: The whole set of cases will deadlock if there is ever an item that no live case wants to consume.
#[pin_project]
pub struct SplitCase<S: Stream, Case> {
    inner: Pin<Rc<PinCell<Peekable<S>>>>,
    /// Tests whether an item from the stream should be consumed
    ///
    /// NOTE: This MUST be total over all `SplitCase`s, otherwise the input stream
    /// will get stuck deadlocked because no candidate tries to consume the item.
    should_consume_item: fn(&S::Item) -> bool,
    /// Narrows the type of the consumed type, using the same precondition as `should_consume_item`.
    ///
    /// NOTE: This MUST return `Some` if `should_consume_item` returns `true`, since we can't put
    /// an item back into the input stream once consumed.
    try_extract_item_case: fn(S::Item) -> Option<Case>,
}

impl<S, Case> Stream for SplitCase<S, Case>
where
    S: Stream,
    S::Item: Debug,
{
    type Item = Case;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.project();
        let mut inner = PinCell::borrow_mut(this.inner.as_ref());
        let inner_peek = PinMut::as_mut(&mut inner).peek();
        pin_mut!(inner_peek);
        match inner_peek.poll(cx) {
            Poll::Ready(Some(x_ref)) => {
                if (this.should_consume_item)(x_ref) {
                    match PinMut::as_mut(&mut inner).poll_next(cx) {
                        Poll::Ready(Some(x)) => Poll::Ready(Some((this.try_extract_item_case)(x).expect(
                            "`try_extract_item_case` returned `None` despite `should_consume_item` returning `true`",
                        ))),
                        res => panic!(
                    "Peekable::poll_next() returned {:?} when Peekable::peek() returned Ready(Some(_))",
                    res
                ),
                    }
                } else {
                    // Handled by another SplitCase instead
                    Poll::Pending
                }
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Splits a `TryStream` into separate `Ok` and `Error` streams.
///
/// Note: This will deadlock if one branch outlives the other
fn trystream_split_result<S>(
    stream: S,
) -> (
    SplitCase<IntoStream<S>, S::Ok>,
    SplitCase<IntoStream<S>, S::Error>,
)
where
    S: TryStream,
    S::Ok: Debug,
    S::Error: Debug,
{
    let stream = Rc::pin(PinCell::new(stream.into_stream().peekable()));
    (
        SplitCase {
            inner: stream.clone(),
            should_consume_item: Result::is_ok,
            try_extract_item_case: Result::ok,
        },
        SplitCase {
            inner: stream,
            should_consume_item: Result::is_err,
            try_extract_item_case: Result::err,
        },
    )
}

/// Forwards Ok elements via a stream built from `make_via_stream`, while passing errors through unmodified
pub fn trystream_try_via<S1, S2>(
    input_stream: S1,
    make_via_stream: impl FnOnce(SplitCase<IntoStream<S1>, S1::Ok>) -> S2,
) -> impl Stream<Item = Result<S2::Ok, S1::Error>>
where
    S1: TryStream,
    S2: TryStream<Error = S1::Error>,
    S1::Ok: Debug,
    S1::Error: Debug,
{
    let (oks, errs) = trystream_split_result(input_stream);
    let via = make_via_stream(oks);
    stream::select(via.into_stream(), errs.map(Err))
}
