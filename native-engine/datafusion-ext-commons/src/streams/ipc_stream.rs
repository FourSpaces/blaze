// Copyright 2022 The Blaze Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt::Debug;

use blaze_commons::{
    jni_call, jni_delete_local_ref, jni_get_object_class, jni_get_string,
    jni_new_direct_byte_buffer, jni_new_global_ref, ResultExt,
};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::error::Result as ArrowResult;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::Result;
use datafusion::physical_plan::common::batch_byte_size;
use datafusion::physical_plan::metrics::{BaselineMetrics, Count};
use datafusion::physical_plan::RecordBatchStream;
use futures::Stream;
use jni::objects::{GlobalRef, JObject};
use jni::sys::{jboolean, jint, jlong, JNI_TRUE};
use std::fs::File;
use std::io::Seek;
use std::io::{Read, SeekFrom};
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use crate::io::read_one_batch;

#[derive(Debug, Clone, Copy)]
pub enum IpcReadMode {
    /// for ConvertToNative
    ChannelUncompressed,

    /// for BroadcastExchange reader
    Channel,

    /// for ShuffleExchange reader
    ChannelAndFileSegment,
}

pub struct IpcReaderStream {
    schema: SchemaRef,
    mode: IpcReadMode,
    segments: GlobalRef,
    reader: Option<RecordBatchReader>,
    baseline_metrics: BaselineMetrics,
    size_counter: Count,
}
unsafe impl Send for IpcReaderStream {}

impl IpcReaderStream {
    pub fn new(
        schema: SchemaRef,
        segments: GlobalRef,
        mode: IpcReadMode,
        baseline_metrics: BaselineMetrics,
        size_counter: Count,
    ) -> IpcReaderStream {
        IpcReaderStream {
            schema,
            mode,
            segments,
            reader: None,
            baseline_metrics,
            size_counter,
        }
    }

    fn next_segment(&mut self) -> Result<bool> {
        let has_next = jni_call!(
            ScalaIterator(self.segments.as_obj()).hasNext() -> jboolean
        )?;
        if has_next != JNI_TRUE {
            self.reader = None;
            return Ok(false);
        }
        let segment = jni_call!(
            ScalaIterator(self.segments.as_obj()).next() -> JObject
        )?;

        let schema = self.schema.clone();
        self.reader = Some(match self.mode {
            IpcReadMode::ChannelUncompressed =>
                get_channel_reader(Some(schema), segment, false)?,

            IpcReadMode::Channel =>
                get_channel_reader(Some(schema), segment, true)?,

            IpcReadMode::ChannelAndFileSegment => {
                let segment_class = jni_get_object_class!(segment)?;
                let segment_classname =
                    jni_call!(Class(segment_class).getName() -> JObject)?;
                let segment_classname = jni_get_string!(segment_classname.into())?;
                if segment_classname == "org.apache.spark.storage.FileSegment" {
                    get_file_segment_reader(Some(schema), segment)?
                } else {
                    get_channel_reader(Some(schema), segment, true)?
                }
            }
        });
        Ok(true)
    }
}

pub fn get_channel_reader(
    schema: Option<SchemaRef>,
    channel: JObject,
    compressed: bool,
) -> Result<RecordBatchReader> {
    let global_ref = jni_new_global_ref!(channel)?;
    jni_delete_local_ref!(channel)?;
    let channel_reader = ReadableByteChannelReader::new(global_ref);

    Ok(RecordBatchReader::new(
        Box::new(channel_reader),
        schema,
        compressed,
    ))
}

pub fn get_file_segment_reader(
    schema: Option<SchemaRef>,
    file_segment: JObject,
) -> Result<RecordBatchReader> {
    let file = jni_call!(SparkFileSegment(file_segment).file() -> JObject)?;
    let path = jni_call!(JavaFile(file).getPath() -> JObject)?;
    let path = jni_get_string!(path.into())?;
    let offset = jni_call!(SparkFileSegment(file_segment).offset() -> jlong)?;
    let length = jni_call!(SparkFileSegment(file_segment).length() -> jlong)?;

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset as u64))?;

    Ok(RecordBatchReader::new(
        Box::new(file.take(length as u64)),
        schema,
        true,
    ))
}

impl Stream for IpcReaderStream {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let elapsed_compute = self.baseline_metrics.elapsed_compute().clone();
        let _timer = elapsed_compute.timer();

        if let Some(reader) = &mut self.reader {
            if let Some(batch) = reader.next_batch()? {
                self.size_counter.add(batch_byte_size(&batch));
                return self.baseline_metrics.record_poll(Poll::Ready(Some(Ok(batch))));
            }
        }

        // current arrow file reader reaches EOF, try next ipc
        if self.next_segment()? {
            return self.poll_next(cx);
        }
        Poll::Ready(None)
    }
}
impl RecordBatchStream for IpcReaderStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

pub struct ReadableByteChannelReader {
    channel: GlobalRef,
    closed: bool,
}
impl ReadableByteChannelReader {
    pub fn new(channel: GlobalRef) -> Self {
        Self {
            channel,
            closed: false,
        }
    }

    pub fn close(&mut self) -> Result<()> {
        if !self.closed {
            jni_call!(JavaReadableByteChannel(self.channel.as_obj()).close() -> ())?;
            self.closed = true;
        }
        Ok(())
    }
}
impl Read for ReadableByteChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.closed {
            return Ok(0);
        }
        let jbuf = jni_new_direct_byte_buffer!(buf).to_io_result()?;

        while jni_call!(JavaBuffer(jbuf).hasRemaining() -> jboolean).to_io_result()? == JNI_TRUE {
            let read_bytes = jni_call!(
                JavaReadableByteChannel(self.channel.as_obj()).read(jbuf) -> jint
            )
            .to_io_result()?;

            if read_bytes < 0 {
                self.close()?;
                break;
            }
        }
        let position = jni_call!(JavaBuffer(jbuf).position() -> jint).to_io_result()?;
        Ok(position as usize)
    }
}
impl Drop for ReadableByteChannelReader {
    fn drop(&mut self) {
        // ensure the channel is closed
        let _ = self.close();
    }
}

pub struct RecordBatchReader {
    input: Box<dyn Read>,
    schema: Option<SchemaRef>,
    compress: bool,
}

impl RecordBatchReader {
    pub fn new(
        input: Box<dyn Read>,
        schema: Option<SchemaRef>,
        compress: bool,
    ) -> Self {
        Self {
            input,
            schema,
            compress,
        }
    }

    pub fn next_batch(&mut self) -> ArrowResult<Option<RecordBatch>> {
        read_one_batch(&mut self.input, self.schema.clone(), self.compress)
    }
}