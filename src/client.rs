//! Asynchronously initiate handshakes.

use std::marker::PhantomData;
use std::mem::uninitialized;
use std::io::ErrorKind::{WriteZero, UnexpectedEof};

use sodiumoxide::crypto::{box_, sign};
use sodiumoxide::utils::memzero;
use futures_core::{Poll, Future};
use futures_core::Async::{Ready, Pending};
use futures_core::task::Context;
use futures_io::{AsyncRead, AsyncWrite, Error};

use crypto::*;
use errors::HandshakeError;

/// Performs the client side of a handshake.
pub struct ClientHandshaker<'a, S>(UnsafeClientHandshaker<S>, PhantomData<&'a u8>);

impl<'a, S: AsyncRead + AsyncWrite> ClientHandshaker<'a, S> {
    /// Creates a new ClientHandshaker to connect to a server with known public key
    /// and app key over the given `stream`.
    pub fn new(stream: S,
               network_identifier: &'a [u8; NETWORK_IDENTIFIER_BYTES],
               client_longterm_pk: &'a sign::PublicKey,
               client_longterm_sk: &'a sign::SecretKey,
               client_ephemeral_pk: &'a box_::PublicKey,
               client_ephemeral_sk: &'a box_::SecretKey,
               server_longterm_pk: &'a sign::PublicKey)
               -> ClientHandshaker<'a, S> {
        ClientHandshaker(UnsafeClientHandshaker::new(stream,
                                                     network_identifier,
                                                     client_longterm_pk,
                                                     client_longterm_sk,
                                                     client_ephemeral_pk,
                                                     client_ephemeral_sk,
                                                     server_longterm_pk),
                         PhantomData)
    }
}

/// Future implementation to asynchronously drive a handshake.
impl<'a, S: AsyncRead + AsyncWrite> Future for ClientHandshaker<'a, S> {
    type Item = (Outcome, S);
    type Error = (HandshakeError, S);

    fn poll(&mut self, cx: &mut Context) -> Poll<Self::Item, Self::Error> {
        self.0.poll(cx)
    }
}

/// Performs the client side of a handshake. This copies the keys so that it isn't constrainted by
/// their lifetime.
pub struct OwningClientHandshaker<S> {
    network_identifier: Box<[u8; NETWORK_IDENTIFIER_BYTES]>,
    client_longterm_pk: Box<sign::PublicKey>,
    client_longterm_sk: Box<sign::SecretKey>,
    client_ephemeral_pk: Box<box_::PublicKey>,
    client_ephemeral_sk: Box<box_::SecretKey>,
    server_longterm_pk: Box<sign::PublicKey>,
    inner: UnsafeClientHandshaker<S>,
}

impl<S: AsyncRead + AsyncWrite> OwningClientHandshaker<S> {
    /// Creates a new OwningClientHandshaker to connect to a server with known public key
    /// and app key over the given `stream`.
    pub fn new(stream: S,
               network_identifier: [u8; NETWORK_IDENTIFIER_BYTES],
               client_longterm_pk: sign::PublicKey,
               client_longterm_sk: sign::SecretKey,
               client_ephemeral_pk: box_::PublicKey,
               client_ephemeral_sk: box_::SecretKey,
               server_longterm_pk: sign::PublicKey)
               -> OwningClientHandshaker<S> {
        let network_identifier = Box::new(network_identifier.clone());
        let client_longterm_pk = Box::new(client_longterm_pk.clone());
        let client_longterm_sk = Box::new(client_longterm_sk.clone());
        let client_ephemeral_pk = Box::new(client_ephemeral_pk.clone());
        let client_ephemeral_sk = Box::new(client_ephemeral_sk.clone());
        let server_longterm_pk = Box::new(server_longterm_pk.clone());

        OwningClientHandshaker {
            inner: UnsafeClientHandshaker::new(stream,
                                               network_identifier.as_ref(),
                                               client_longterm_pk.as_ref(),
                                               client_longterm_sk.as_ref(),
                                               client_ephemeral_pk.as_ref(),
                                               client_ephemeral_sk.as_ref(),
                                               server_longterm_pk.as_ref()),
            network_identifier,
            client_longterm_pk,
            client_longterm_sk,
            client_ephemeral_pk,
            client_ephemeral_sk,
            server_longterm_pk,
        }
    }
}

/// Future implementation to asynchronously drive a handshake.
impl<S: AsyncRead + AsyncWrite> Future for OwningClientHandshaker<S> {
    type Item = (Outcome, S);
    type Error = (HandshakeError, S);

    fn poll(&mut self, cx: &mut Context) -> Poll<Self::Item, Self::Error> {
        self.inner.poll(cx)
    }
}

// Performs the client side of a handshake.
struct UnsafeClientHandshaker<S> {
    stream: Option<S>,
    client: Client,
    state: State,
    data: [u8; MSG3_BYTES], // used to hold and cache the results of `client.create_client_challenge` and `client.create_client_auth`, and any data read from the server
    offset: usize, // offset into the data array at which to read/write
}

impl<S: AsyncRead + AsyncWrite> UnsafeClientHandshaker<S> {
    // Creates a new UnsafeClientHandshaker to connect to a server with known public key
    // and app key over the given `stream`.
    fn new(stream: S,
           network_identifier: *const [u8; NETWORK_IDENTIFIER_BYTES],
           client_longterm_pk: *const sign::PublicKey,
           client_longterm_sk: *const sign::SecretKey,
           client_ephemeral_pk: *const box_::PublicKey,
           client_ephemeral_sk: *const box_::SecretKey,
           server_longterm_pk: *const sign::PublicKey)
           -> UnsafeClientHandshaker<S> {
        unsafe {
            let mut ret = UnsafeClientHandshaker {
                stream: Some(stream),
                client: Client::new(network_identifier,
                                    &(*client_longterm_pk).0,
                                    &(*client_longterm_sk).0,
                                    &(*client_ephemeral_pk).0,
                                    &(*client_ephemeral_sk).0,
                                    &(*server_longterm_pk).0),
                state: WriteMsg1,
                data: [0; MSG3_BYTES],
                offset: 0,
            };
            ret.client
                .create_msg1(&mut *(&mut ret.data as *mut [u8; MSG3_BYTES] as
                                    *mut [u8; MSG1_BYTES]));

            ret
        }
    }
}

// Zero buffered handshake data on dropping.
impl<S> Drop for UnsafeClientHandshaker<S> {
    fn drop(&mut self) {
        memzero(&mut self.data);
    }
}

// Future implementation to asynchronously drive a handshake.
impl<S: AsyncRead + AsyncWrite> Future for UnsafeClientHandshaker<S> {
    type Item = (Outcome, S);
    type Error = (HandshakeError, S);

    fn poll(&mut self, cx: &mut Context) -> Poll<Self::Item, Self::Error> {
        let mut stream = self.stream
            .take()
            .expect("Polled UnsafeClientHandshaker after completion");

        match self.state {
            WriteMsg1 => {
                while self.offset < MSG1_BYTES {
                    match stream.poll_write(cx, &self.data[self.offset..MSG1_BYTES]) {
                        Ok(Ready(written)) => {
                            if written == 0 {
                                return Err((Error::new(WriteZero, "failed to write msg1").into(),
                                            stream));
                            }
                            self.offset += written;
                        }
                        Ok(Pending) => {
                            self.stream = Some(stream);
                            return Ok(Pending);
                        }
                        Err(e) => return Err((e.into(), stream)),
                    }
                }

                self.stream = Some(stream);
                self.offset = 0;
                self.state = FlushMsg1;

                return self.poll(cx);
            }

            FlushMsg1 => {
                match stream.poll_flush(cx) {
                    Ok(Ready(())) => {}
                    Ok(Pending) => {
                        self.stream = Some(stream);
                        return Ok(Pending);
                    }
                    Err(e) => return Err((e.into(), stream)),
                }

                self.stream = Some(stream);
                self.state = ReadMsg2;
                return self.poll(cx);
            }

            ReadMsg2 => {
                while self.offset < MSG2_BYTES {
                    match stream.poll_read(cx, &mut self.data[self.offset..MSG2_BYTES]) {
                        Ok(Ready(read)) => {
                            if read == 0 {
                                return Err((Error::new(UnexpectedEof, "failed to read msg2")
                                                .into(),
                                            stream));
                            }
                            self.offset += read;
                        }
                        Ok(Pending) => {
                            self.stream = Some(stream);
                            return Ok(Pending);
                        }
                        Err(e) => return Err((e.into(), stream)),
                    }
                }

                if !self.client
                        .verify_msg2(unsafe {
                                         &*(&self.data as *const [u8; MSG3_BYTES] as
                                            *const [u8; MSG2_BYTES])
                                     }) {
                    return Err((HandshakeError::CryptoError, stream));
                }

                self.stream = Some(stream);
                self.offset = 0;
                self.state = WriteMsg3;
                self.client.create_msg3(&mut self.data);
                return self.poll(cx);
            }

            WriteMsg3 => {
                while self.offset < MSG3_BYTES {
                    match stream.poll_write(cx, &self.data[self.offset..MSG3_BYTES]) {
                        Ok(Ready(written)) => {
                            if written == 0 {
                                return Err((Error::new(WriteZero, "failed to write msg3").into(),
                                            stream));
                            }
                            self.offset += written;
                        }
                        Ok(Pending) => {
                            self.stream = Some(stream);
                            return Ok(Pending);
                        }
                        Err(e) => return Err((e.into(), stream)),
                    }
                }

                self.stream = Some(stream);
                self.offset = 0;
                self.state = FlushMsg3;
                return self.poll(cx);
            }

            FlushMsg3 => {
                match stream.poll_flush(cx) {
                    Ok(Ready(())) => {}
                    Ok(Pending) => {
                        self.stream = Some(stream);
                        return Ok(Pending);
                    }
                    Err(e) => return Err((e.into(), stream)),
                }

                self.stream = Some(stream);
                self.state = ReadMsg4;
                return self.poll(cx);
            }

            ReadMsg4 => {
                while self.offset < MSG4_BYTES {
                    match stream.poll_read(cx, &mut self.data[self.offset..MSG4_BYTES]) {
                        Ok(Ready(read)) => {
                            if read == 0 {
                                return Err((Error::new(UnexpectedEof, "failed to read msg4")
                                                .into(),
                                            stream));
                            }
                            self.offset += read;
                        }
                        Ok(Pending) => {
                            self.stream = Some(stream);
                            return Ok(Pending);
                        }
                        Err(e) => return Err((e.into(), stream)),
                    }
                }

                if !self.client
                        .verify_msg4(unsafe {
                                         &*(&self.data as *const [u8; MSG3_BYTES] as
                                            *const [u8; MSG4_BYTES])
                                     }) {
                    return Err((HandshakeError::CryptoError, stream));
                }

                let mut outcome = unsafe { uninitialized() };
                self.client.outcome(&mut outcome);
                return Ok(Ready((outcome, stream)));
            }
        }
    }
}

// State for the future state machine.
enum State {
    WriteMsg1,
    FlushMsg1,
    ReadMsg2,
    WriteMsg3,
    FlushMsg3,
    ReadMsg4,
}
use client::State::*;
