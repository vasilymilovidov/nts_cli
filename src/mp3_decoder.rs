use std::{collections::VecDeque, io::Read, time::Duration};

use minimp3::{Decoder, Frame};
use rodio::Source;

/// This is a modified version of [rodio's Mp3Decoder](https://github.com/RustAudio/rodio/blob/55d957f8b40c59fccea4162c4b03f6dd87a7a4d9/src/decoder/mp3.rs)
/// which removes the "Seek" trait bound for streaming network audio.
///
/// Related GitHub issue:
/// https://github.com/RustAudio/rodio/issues/333

pub struct Mp3StreamDecoder<R>
where
    R: Read,
{
    decoder: Decoder<R>,
    current_frame: Frame,
    current_frame_offset: usize,
    buffer: VecDeque<i16>,
    buffer_size: usize,
}

impl<R> Mp3StreamDecoder<R>
where
    R: Read,
{
    pub fn new(mut data: R, buffer_size: usize) -> Result<Self, R> {
        if !is_mp3(data.by_ref()) {
            return Err(data);
        }
        let mut decoder = Decoder::new(data);
        let current_frame = decoder.next_frame().unwrap();

        let mut decoder = Self {
            decoder,
            current_frame,
            current_frame_offset: 0,
            buffer: VecDeque::with_capacity(buffer_size),
            buffer_size,
        };

        // Pre-fill the buffer
        decoder.fill_buffer();

        Ok(decoder)
    }

    // pub fn into_inner(self) -> R {
    //     self.decoder.into_inner()
    // }

    fn fill_buffer(&mut self) {
        while self.buffer.len() < self.buffer_size {
            if self.current_frame_offset == self.current_frame.data.len() {
                match self.decoder.next_frame() {
                    Ok(frame) => self.current_frame = frame,
                    _ => break,
                }
                self.current_frame_offset = 0;
            }

            while self.current_frame_offset < self.current_frame.data.len() && self.buffer.len() < self.buffer_size {
                self.buffer.push_back(self.current_frame.data[self.current_frame_offset]);
                self.current_frame_offset += 1;
            }
        }
    }
}

impl<R> Source for Mp3StreamDecoder<R>
where
    R: Read,
{
    #[inline]
    fn current_frame_len(&self) -> Option<usize> {
        Some(self.buffer.len())
    }

    #[inline]
    fn channels(&self) -> u16 {
        self.current_frame.channels as _
    }

    #[inline]
    fn sample_rate(&self) -> u32 {
        self.current_frame.sample_rate as _
    }

    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

impl<R> Iterator for Mp3StreamDecoder<R>
where
    R: Read,
{
    type Item = i16;

    #[inline]
    fn next(&mut self) -> Option<i16> {
        if self.buffer.is_empty() {
            self.fill_buffer();
        }

        self.buffer.pop_front()
    }
}

/// Always returns true.
fn is_mp3<R>(_data: R) -> bool
where
    R: Read,
{
    true
}