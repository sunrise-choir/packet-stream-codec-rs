//! Implements the [packet-stream-codec](https://github.com/dominictarr/packet-stream-codec)
//! used by muxrpc in rust.
#![deny(missing_docs)]

#[macro_use]
extern crate futures;
extern crate tokio_io;
#[macro_use]
extern crate atm_io_utils;

#[cfg(test)]
extern crate partial_io;
#[cfg(test)]
extern crate quickcheck;
#[cfg(test)]
extern crate async_ringbuffer;
#[cfg(test)]
extern crate rand;

use std::io;
use std::io::ErrorKind::{WriteZero, UnexpectedEof, InvalidData, InvalidInput, WouldBlock,
                         Interrupted};
use std::mem::transmute;
use std::slice::from_raw_parts_mut;

use futures::{Sink, Stream, Poll, StartSend, AsyncSink, Async};
use tokio_io::{AsyncRead, AsyncWrite};

/// The ids used by packet-stream packets
pub type PacketId = i32;

/// The metadata of a packet.
#[derive(Debug, Copy, Clone)]
pub struct Metadata {
    /// Flags indicate the type of the data in the packet, whether the packet is
    /// a request, and wether it signals an end/error.
    pub flags: u8,
    /// The id of the packet.
    pub id: PacketId,
}

impl Metadata {
    /// Returns true if the stream flag of the packet is set.
    pub fn is_stream_packet(&self) -> bool {
        self.flags & STREAM != 0
    }

    /// Returns true if the end/error flag of the packet is set.
    pub fn is_end_packet(&self) -> bool {
        self.flags & END != 0
    }

    /// Returns true if the type flags signal a buffer.
    pub fn is_buffer_packet(&self) -> bool {
        self.flags & TYPE == TYPE_BINARY
    }

    /// Returns true if the type flags signal a string.
    pub fn is_string_packet(&self) -> bool {
        self.flags & TYPE == TYPE_STRING
    }

    /// Returns true if the type flags signal json.
    pub fn is_json_packet(&self) -> bool {
        self.flags & TYPE == TYPE_JSON
    }

    /// Returns true if the type flags signal the unused type.
    ///
    /// A `CodecStream` returns an error if it encounters a paket with this type,
    /// so this returns false for all `Metadata`s yielded from a `CodecStream`.
    pub fn is_unused_packet(&self) -> bool {
        self.flags & TYPE == TYPE_UNUSED
    }

    fn to_be(self) -> Metadata {
        Metadata {
            flags: self.flags.to_be(),
            id: self.id.to_be(),
        }
    }
}

///  Bitmask for the stream flag.
pub static STREAM: u8 = 0b0000_1000;
///  Bitmask for the end flag.
pub static END: u8 = 0b0000_0100;
///  Bitmask for the type flags.
pub static TYPE: u8 = 0b0000_0011;

/// Value of the binary type.
pub static TYPE_BINARY: u8 = 0;
/// Value of the string type.
pub static TYPE_STRING: u8 = 1;
/// Value of the json type.
pub static TYPE_JSON: u8 = 2;
/// The unused fourth possible value.
static TYPE_UNUSED: u8 = 3;

const ZEROS: [u8; 9] = [0u8, 0, 0, 0, 0, 0, 0, 0, 0];

enum SinkState {
    // initial and after flushing
    Idle,
    // send data down a reader
    Buffering(WritePacketState),
    // send the end-of-stream header
    EndOfStream(u8),
    // shut down the wrapped AsyncWrite
    Shutdown,
}

// State for actually writing data.
#[derive(Debug, Copy, Clone)]
enum WritePacketState {
    Flags(Metadata),
    Length(PacketId, u8), // u8 signifies how many bytes of the length have been written
    Id(PacketId, u8), // u8 signifies how many bytes of the id have been written
    Data(u32), // u32 signifies how many bytes of the packet have been written
}

/// This sink consumes pairs of `Metadata` and `AsRef<[u8]>`s of type `B` and
/// encodes them into the wrapped `AsyncWrite` of type `W`.
pub struct CodecSink<W, B> {
    writer: W,
    bytes: Option<B>,
    state: SinkState,
}

impl<W, B> CodecSink<W, B> {
    /// Create a new `CodecSink`, wrapping the given writer.
    pub fn new(writer: W) -> CodecSink<W, B> {
        CodecSink {
            writer,
            bytes: None,
            state: SinkState::Idle,
        }
    }

    /// Consume the `CodecSink` to retrieve ownership of the inner writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W, B> Sink for CodecSink<W, B>
    where W: AsyncWrite,
          B: AsRef<[u8]>
{
    /// The length of the [u8] may not be larger than `u32::max_value()`.
    /// Otherwise, `start_send` returns an error of kind `InvalidInput`.
    type SinkItem = (B, Metadata);
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.state {
            SinkState::Idle => {
                if item.0.as_ref().len() as u32 > u32::max_value() {
                    Err(io::Error::new(InvalidInput, "item too large for packet-stream-codec"))
                } else {
                    self.bytes = Some(item.0);
                    self.state = SinkState::Buffering(WritePacketState::Flags(item.1.to_be()));
                    self.poll_complete().map(|_| AsyncSink::Ready)
                }
            }

            SinkState::Buffering(_) => {
                match self.poll_complete() {
                    Ok(Async::Ready(_)) => self.start_send(item),
                    Ok(Async::NotReady) => Ok(AsyncSink::NotReady(item)),
                    Err(e) => Err(e),
                }
            }

            SinkState::EndOfStream(_) |
            SinkState::Shutdown => panic!("Called start_send on CodecSink after calling close"),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        match self.state {
            SinkState::Idle => Ok(Async::Ready(retry_nb!(self.writer.flush()))),

            SinkState::Buffering(state) => {
                match state {
                    WritePacketState::Flags(Metadata { flags, id }) => {
                        let written = retry_nb!(self.writer.write(&[flags]));

                        if written == 0 {
                            Err(io::Error::new(WriteZero, "failed to write packet flags"))
                        } else {
                            debug_assert!(written == 1);
                            self.state = SinkState::Buffering(WritePacketState::Length(id, 0));
                            self.poll_complete()
                        }
                    }

                    WritePacketState::Length(id, mut offset) => {
                        let len_bytes = unsafe {
                            transmute::<_, [u8; 4]>((self.bytes.as_ref().unwrap().as_ref().len() as
                                                     u32)
                                                            .to_be())
                        };

                        while offset < 4 {
                            let written = retry_nb!(self.writer.write(&len_bytes[offset as
                                                                       usize..]));

                            if written == 0 {
                                return Err(io::Error::new(WriteZero,
                                                          "failed to write packet length"));
                            } else {
                                offset += written as u8;
                                self.state = SinkState::Buffering(WritePacketState::Length(id,
                                                                                           offset));
                            }
                        }

                        self.state = SinkState::Buffering(WritePacketState::Id(id, 0));
                        self.poll_complete()
                    }

                    WritePacketState::Id(id, mut offset) => {
                        let id_bytes = unsafe { transmute::<_, [u8; 4]>(id) };
                        while offset < 4 {
                            let written = retry_nb!(self.writer.write(&id_bytes[offset as
                                                                       usize..]));

                            if written == 0 {
                                return Err(io::Error::new(WriteZero, "failed to write packet id"));
                            } else {
                                offset += written as u8;
                                self.state = SinkState::Buffering(WritePacketState::Id(id, offset));
                            }
                        }

                        self.state = SinkState::Buffering(WritePacketState::Data(0));
                        self.poll_complete()
                    }

                    WritePacketState::Data(mut offset) => {
                        {
                            let packet_ref = self.bytes.as_ref().unwrap().as_ref();

                            while (offset as usize) < packet_ref.len() {
                                let written =
                                    retry_nb!(self.writer.write(&packet_ref[offset as usize..]));

                                if written == 0 {
                                    return Err(io::Error::new(WriteZero,
                                                              "failed to write packet data"));
                                } else {
                                    offset += written as u32;
                                    self.state =
                                        SinkState::Buffering(WritePacketState::Data(offset));
                                }
                            }
                        }

                        self.state = SinkState::Idle;
                        self.poll_complete()
                    }
                }
            }

            SinkState::EndOfStream(_) |
            SinkState::Shutdown => self.close(),
        }
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        match self.state {
            SinkState::Idle => {
                self.state = SinkState::EndOfStream(0);
                self.close()
            }

            SinkState::Buffering(_) => {
                try_ready!(self.poll_complete());
                self.state = SinkState::EndOfStream(0);
                self.close()
            }

            SinkState::EndOfStream(mut offset) => {
                while offset < 9 {
                    let written = retry_nb!(self.writer.write(&ZEROS[offset as usize..]));

                    if written == 0 {
                        return Err(io::Error::new(WriteZero,
                                                  "failed to write end-of-stream header"));
                    } else {
                        offset += written as u8;
                        self.state = SinkState::EndOfStream(offset);
                    }
                }

                self.state = SinkState::Shutdown;
                self.close()
            }

            SinkState::Shutdown => self.writer.shutdown(),
        }
    }
}

enum StreamState {
    // read the flags of the packet
    Flags,
    // read the length of the packet (state is the read offset, and a buffer to read into)
    Length(u8, [u8; 4]),
    // read the id of the packet (state is the read offset, a buffer to read into,
    // and the length of the currently decoded packet)
    Id(u8, [u8; 4], u32),
    // read the actual data of the packet (state is the length of the currently decoded packet)
    Data(u32),
}

/// This stream decodes pairs of data and metadata from the wrapped
/// `AsyncRead` of type `R`.
pub struct CodecStream<R> {
    reader: R,
    state: StreamState,
    metadata: Metadata,
    data: Option<Vec<u8>>,
}

impl<R> CodecStream<R> {
    /// Create a new `CodecStream`, wrapping the given reader.
    pub fn new(reader: R) -> CodecStream<R> {
        CodecStream {
            reader,
            state: StreamState::Flags,
            metadata: Metadata { flags: 0, id: 0 },
            data: None,
        }
    }

    /// Consume the `CodecStream` to retrieve ownership of the inner reader.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

impl<R: AsyncRead> Stream for CodecStream<R> {
    type Item = (Box<[u8]>, Metadata);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.state {
            StreamState::Flags => {
                let mut flags_buf = [0u8; 1];
                let read = retry_nb!(self.reader.read(&mut flags_buf));

                if read == 0 {
                    Err(io::Error::new(UnexpectedEof, "failed to read packet flags"))
                } else {
                    self.metadata.flags = u8::from_be(flags_buf[0]);
                    if self.metadata.is_unused_packet() {
                        Err(io::Error::new(InvalidData, "read packet with invalid type flag"))
                    } else {
                        self.state = StreamState::Length(0, [0; 4]);
                        self.poll()
                    }
                }
            }

            StreamState::Length(mut offset, mut length_buf) => {
                while offset < 4 {
                    let read = retry_nb!(self.reader.read(&mut length_buf[offset as usize..]));

                    if read == 0 {
                        return Err(io::Error::new(UnexpectedEof, "failed to read packet length"));
                    } else {
                        offset += read as u8;
                        self.state = StreamState::Length(offset, length_buf);
                    }
                }

                let length = u32::from_be(unsafe { transmute::<[u8; 4], u32>(length_buf) });
                self.state = StreamState::Id(0, [0; 4], length);
                self.poll()
            }

            StreamState::Id(mut offset, mut id_buf, length) => {
                while offset < 4 {
                    let read = retry_nb!(self.reader.read(&mut id_buf[offset as usize..]));

                    if read == 0 {
                        return Err(io::Error::new(UnexpectedEof, "failed to read packet id"));
                    } else {
                        offset += read as u8;
                        self.state = StreamState::Id(offset, id_buf, length);
                    }
                }

                let id = i32::from_be(unsafe { transmute::<[u8; 4], i32>(id_buf) });
                self.metadata.id = id;

                if (length == 0) && (self.metadata.flags == 0) && (self.metadata.id == 0) {
                    return Ok(Async::Ready(None));
                }

                self.data = Some(Vec::with_capacity(length as usize));
                self.state = StreamState::Data(length);
                self.poll()
            }

            StreamState::Data(length) => {
                let mut data = self.data.take().unwrap();
                let mut old_len = data.len();

                let capacity = data.capacity();
                let data_ptr = data.as_mut_slice().as_mut_ptr();
                let data_slice = unsafe { from_raw_parts_mut(data_ptr, capacity) };

                while old_len < length as usize {
                    match self.reader.read(&mut data_slice[old_len..]) {
                        Ok(0) => {
                            return Err(io::Error::new(UnexpectedEof,
                                                      "failed to read whole packet content"));
                        }
                        Ok(read) => {
                            unsafe { data.set_len(old_len + read) };
                            old_len += read;
                        }
                        Err(ref e) if e.kind() == WouldBlock => {
                            self.data = Some(data);
                            return Ok(Async::NotReady);
                        }
                        Err(ref e) if e.kind() == Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }

                self.state = StreamState::Flags;
                return Ok(Async::Ready(Some((data.into_boxed_slice(), self.metadata))));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Error;

    use partial_io::{PartialAsyncRead, PartialAsyncWrite, PartialWithErrors};
    use partial_io::quickcheck_types::GenInterruptedWouldBlock;
    use quickcheck::{QuickCheck, StdGen};
    use async_ringbuffer::*;
    use rand;
    use futures::stream::iter_ok;
    use futures::Future;

    #[test]
    fn codec_sink_stream() {
        let rng = StdGen::new(rand::thread_rng(), 2000); // TODO 2000
        let mut quickcheck = QuickCheck::new().gen(rng).tests(200);
        quickcheck.quickcheck(test_codec_sink_stream as
                              fn(usize,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 PartialWithErrors<GenInterruptedWouldBlock>,
                                 Vec<u8>)
                                 -> bool);
    }

    fn test_codec_sink_stream(buf_size: usize,
                              write_ops: PartialWithErrors<GenInterruptedWouldBlock>,
                              read_ops: PartialWithErrors<GenInterruptedWouldBlock>,
                              data: Vec<u8>)
                              -> bool {
        let expected_data = data.clone();

        let (writer, reader) = ring_buffer(buf_size + 1);
        let writer = PartialAsyncWrite::new(writer, write_ops);
        let reader = PartialAsyncRead::new(reader, read_ops);

        let sink = CodecSink::new(writer);
        let stream = CodecStream::new(reader);

        let send = sink.send_all(iter_ok::<_, Error>((0..data.len()).map(|i| {
                                                                             (vec![data[i]],
                                                                              Metadata {
                                                                                  flags: 0,
                                                                                  id: i as PacketId,
                                                                              })
                                                                         })));

        let (received, _) = stream.collect().join(send).wait().unwrap();

        for (i, &(ref data, ref metadata)) in received.iter().enumerate() {
            if (i as PacketId) != metadata.id || metadata.is_stream_packet() ||
               metadata.is_end_packet() ||
               (!metadata.is_buffer_packet() ||
                data != &vec![expected_data[i]].into_boxed_slice()) {
                return false;
            }
        }

        return true;
    }
}
