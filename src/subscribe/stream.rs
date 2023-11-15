use super::{new_socket_internal, recv_internal};
use crate::{error::Result, event::SocketEvent, message::Message, EventMessage, DATA_MAX_LEN};
use async_zmq::{Stream, StreamExt, Subscribe};
use core::{
    future::Future,
    mem,
    pin::{pin, Pin},
    slice,
    task::{Context as AsyncContext, Poll, Waker},
    time::Duration,
};
use futures_util::{
    future::{select, Either},
    stream::FusedStream,
};
use std::{
    sync::{Arc, Mutex},
    thread,
};

/// Stream that asynchronously produces [`Message`]s using a ZMQ subscriber.
pub struct MessageStream {
    zmq_stream: Subscribe,
    data_cache: Box<[u8; DATA_MAX_LEN]>,
}

impl MessageStream {
    fn new(zmq_stream: Subscribe) -> Self {
        Self {
            zmq_stream,
            data_cache: vec![0; DATA_MAX_LEN].into_boxed_slice().try_into().unwrap(),
        }
    }

    /// Returns a reference to the ZMQ socket used by this stream. To get the [`zmq::Socket`], use
    /// [`as_raw_socket`] on the result. This is useful to set socket options or use other
    /// functions provided by [`zmq`] or [`async_zmq`].
    ///
    /// [`as_raw_socket`]: Subscribe::as_raw_socket
    pub fn as_zmq_socket(&self) -> &Subscribe {
        &self.zmq_stream
    }
}

impl Stream for MessageStream {
    type Item = Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut AsyncContext<'_>) -> Poll<Option<Self::Item>> {
        self.zmq_stream.poll_next_unpin(cx).map(|opt| {
            opt.map(|res| match res {
                Ok(mp) => recv_internal(mp.iter(), &mut self.data_cache),
                Err(err) => Err(err.into()),
            })
        })
    }
}

impl FusedStream for MessageStream {
    fn is_terminated(&self) -> bool {
        false
    }
}

// TODO move, name
pub enum SocketMessage {
    Message(Message),
    Event(EventMessage),
}

// The generic type params don't matter as this will only be used for receiving
type Pair = async_zmq::Pair<std::vec::IntoIter<&'static [u8]>, &'static [u8]>;

// TODO name?
pub struct SocketMessageStream {
    messages: MessageStream,
    monitor: Pair,
}

impl SocketMessageStream {
    fn new(messages: MessageStream, monitor: Pair) -> Self {
        Self { messages, monitor }
    }
}

impl Stream for SocketMessageStream {
    type Item = Result<SocketMessage>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut AsyncContext<'_>) -> Poll<Option<Self::Item>> {
        match self.monitor.poll_next_unpin(cx) {
            Poll::Ready(msg) => {
                // TODO properly handle errors (review uses of unwrap, expect, unreachable)
                return Poll::Ready(Some(Ok(SocketMessage::Event(EventMessage::parse_from(
                    msg.unwrap()?,
                )))));
            }
            Poll::Pending => {}
        }

        self.messages
            .poll_next_unpin(cx)
            .map(|opt| opt.map(|res| res.map(SocketMessage::Message)))
    }
}

impl FusedStream for SocketMessageStream {
    fn is_terminated(&self) -> bool {
        false
    }
}

// TODO name, disconnect on failure?
pub struct FiniteMessageStream {
    inner: Option<SocketMessageStream>,
}

impl FiniteMessageStream {
    pub fn new(inner: SocketMessageStream) -> Self {
        Self { inner: Some(inner) }
    }
}

impl Stream for FiniteMessageStream {
    type Item = Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut AsyncContext<'_>) -> Poll<Option<Self::Item>> {
        if let Some(inner) = &mut self.inner {
            loop {
                match inner.poll_next_unpin(cx) {
                    Poll::Ready(opt) => match opt.unwrap()? {
                        SocketMessage::Message(msg) => return Poll::Ready(Some(Ok(msg))),
                        SocketMessage::Event(EventMessage { event, .. }) => {
                            if let SocketEvent::Disconnected { .. } = event {
                                // drop to disconnect
                                self.inner = None;
                                return Poll::Ready(None);
                            } else {
                                // only here it loops
                            }
                        }
                    },
                    Poll::Pending => return Poll::Pending,
                }
            }
        } else {
            Poll::Ready(None)
        }
    }
}

impl FusedStream for FiniteMessageStream {
    fn is_terminated(&self) -> bool {
        self.inner.is_none()
    }
}

/// Stream that asynchronously produces [`Message`]s using multiple ZMQ subscribers. The ZMQ
/// sockets are polled in a round-robin fashion.
#[deprecated(
    since = "1.3.2",
    note = "This struct is only used by deprecated functions."
)]
pub struct MultiMessageStream(pub MessageStream);

#[allow(deprecated)]
impl MultiMessageStream {
    /// Returns a reference to the separate [`MessageStream`]s this [`MultiMessageStream`] is made
    /// of. This is useful to set socket options or use other functions provided by [`zmq`] or
    /// [`async_zmq`]. (See [`MessageStream::as_zmq_socket`])
    pub fn as_streams(&self) -> &[MessageStream] {
        slice::from_ref(&self.0)
    }

    /// Returns the separate [`MessageStream`]s this [`MultiMessageStream`] is made of. This is
    /// useful to set socket options or use other functions provided by [`zmq`] or [`async_zmq`].
    /// (See [`MessageStream::as_zmq_socket`])
    pub fn into_streams(self) -> Vec<MessageStream> {
        vec![self.0]
    }
}

#[allow(deprecated)]
impl Stream for MultiMessageStream {
    type Item = Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut AsyncContext<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_next_unpin(cx)
    }
}

#[allow(deprecated)]
impl FusedStream for MultiMessageStream {
    fn is_terminated(&self) -> bool {
        false
    }
}

/// Subscribes to multiple ZMQ endpoints and returns a [`MultiMessageStream`].
#[deprecated(
    since = "1.3.2",
    note = "Use subscribe_async. This function has no performance benefit over subscribe_single_async anymore."
)]
#[allow(deprecated)]
pub fn subscribe_multi_async(endpoints: &[&str]) -> Result<MultiMessageStream> {
    subscribe_async(endpoints).map(MultiMessageStream)
}

/// Subscribes to a single ZMQ endpoint and returns a [`MessageStream`].
#[deprecated(
    since = "1.3.2",
    note = "Use subscribe_async. The name changed because there is no distinction made anymore between subscribing to 1 or more endpoints."
)]
pub fn subscribe_single_async(endpoint: &str) -> Result<MessageStream> {
    subscribe_async(&[endpoint])
}

/// Subscribes to multiple ZMQ endpoints and returns a [`MessageStream`].
pub fn subscribe_async(endpoints: &[&str]) -> Result<MessageStream> {
    let (_context, socket) = new_socket_internal(endpoints)?;

    Ok(MessageStream::new(socket.into()))
}

// TODO split up this file?, these type of functions for receiver and blocking?
// TODO doc (also other functions, structs, etc)
pub fn subscribe_async_monitor(endpoints: &[&str]) -> Result<SocketMessageStream> {
    let (context, socket) = new_socket_internal(endpoints)?;

    socket.monitor("inproc://monitor", zmq::SocketEvent::ALL as i32)?;

    let monitor = context.socket(zmq::PAIR)?;
    monitor.connect("inproc://monitor")?;

    Ok(SocketMessageStream::new(
        MessageStream::new(socket.into()),
        monitor.into(),
    ))
}

// TODO have some way to extract connecting to which endpoints failed, now just a (unit) error is returned (by tokio::time::timeout)

// pub struct SubscribeWaitHandshakeFuture {
//     stream: Option<SocketMessageStream>,
//     connecting: usize,
//     next_message: Next<'static, Pair>,
// }

// impl Future for SubscribeWaitHandshakeFuture {
//     type Output = Result<FiniteMessageStream>;

//     fn poll(self: Pin<&mut Self>, cx: &mut AsyncContext<'_>) -> Poll<Self::Output> {
//         todo!();
//     }
// }

// // TODO doc, test
// /// returns a stream after a successful handshake, that stops returning messages when disconnected
// pub fn subscribe_async_wait_handshake(endpoints: &[&str]) -> Result<SubscribeWaitHandshakeFuture> {
//     let mut stream = subscribe_async_monitor(endpoints)?;
//     let mut connecting = endpoints.len();
//     let next_message = stream.monitor.next();

//     Ok(SubscribeWaitHandshakeFuture {
//         stream: Some(stream),
//         connecting,
//         next_message,
//     })
// }

// TODO doc, test
/// returns a stream after a successful handshake, that stops returning messages when disconnected.
/// this should be used with the timeout function of your async runtime, this function will wait
/// indefinitely. to runtime independently return after some timeout, a second thread is needed
/// which is inefficient
pub async fn subscribe_async_wait_handshake(endpoints: &[&str]) -> Result<FiniteMessageStream> {
    let mut stream = subscribe_async_monitor(endpoints)?;
    let mut connecting = endpoints.len();

    if connecting == 0 {
        return Ok(FiniteMessageStream::new(stream));
    }

    loop {
        // TODO only decode first frame, the second frame (source address) is unused here but a String is allocated for it
        match EventMessage::parse_from(stream.monitor.next().await.unwrap()?).event {
            SocketEvent::HandshakeSucceeded => {
                connecting -= 1;
            }
            SocketEvent::Disconnected { .. } => {
                connecting += 1;
            }
            _ => {
                continue;
            }
        }
        if connecting == 0 {
            return Ok(FiniteMessageStream::new(stream));
        }
    }
}

// TODO doc, is this inefficient function even useful?, test
pub async fn subscribe_async_wait_handshake_timeout(
    endpoints: &[&str],
    timeout: Duration,
) -> Option<Result<FiniteMessageStream>> {
    let subscribe = subscribe_async_wait_handshake(endpoints);
    let timeout = sleep(timeout);

    match select(pin!(subscribe), timeout).await {
        Either::Left((res, _)) => Some(res),
        Either::Right(_) => None,
    }
}

fn sleep(dur: Duration) -> Sleep {
    let state = Arc::new(Mutex::new(SleepReadyState::Pending));
    {
        let state = state.clone();
        thread::spawn(move || {
            thread::sleep(dur);
            let state = {
                let mut g = state.lock().unwrap();
                mem::replace(&mut *g, SleepReadyState::Done)
            };
            if let SleepReadyState::PendingPolled(waker) = state {
                waker.wake();
            }
        });
    }

    Sleep(state)
}

enum SleepReadyState {
    Pending,
    PendingPolled(Waker),
    Done,
}

struct Sleep(Arc<Mutex<SleepReadyState>>);

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut AsyncContext<'_>) -> Poll<Self::Output> {
        let mut g = self.0.lock().unwrap();
        if let SleepReadyState::Done = *g {
            Poll::Ready(())
        } else {
            *g = SleepReadyState::PendingPolled(cx.waker().clone());
            Poll::Pending
        }
    }
}
