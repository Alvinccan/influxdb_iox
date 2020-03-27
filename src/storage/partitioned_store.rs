//! partitioned_store is a trait and set of helper functions and structs to define Partitions
//! that store data. The helper funcs and structs merge results from multiple partitions together.
use crate::delorean::{Predicate, TimestampRange};
use crate::line_parser::PointType;
use crate::storage::series_store::ReadPoint;
use crate::storage::StorageError;

use futures::stream::{BoxStream, Stream};
use std::cmp::Ordering;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A Partition is a block of data. It has methods for reading the metadata like which measurements,
/// tags, tag values, and fields exist. Along with the raw time series data. It is designed to work
/// as a stream so that it can be used in safely an asynchronous context. A partition is the
/// lowest level organization scheme. Above it you will have a database which keeps track of
/// what organizations and buckets exist. A bucket will have 1-many partitions and a partition
/// will only ever contain data for a single bucket.
pub trait Partition {
    fn id(&self) -> String;

    fn size(&self) -> u64;

    fn write(&self, points: &[PointType]) -> Result<(), StorageError>;

    fn get_tag_keys(
        &self,
        range: &TimestampRange,
        predicate: &Predicate,
    ) -> Result<BoxStream<'_, String>, StorageError>;

    fn get_tag_values(
        &self,
        tag_key: &str,
        range: &TimestampRange,
        predicate: &Predicate,
    ) -> Result<BoxStream<'_, String>, StorageError>;

    fn read(
        &self,
        batch_size: usize,
        predicate: &Predicate,
        range: &TimestampRange,
    ) -> Result<BoxStream<'_, ReadBatch>, StorageError>;
}

/// StringMergeStream will do a merge sort with deduplication of multiple streams of Strings. This
/// is used for combining results from multiple partitions for calls to get measurements, tag keys,
/// tag values, or field keys. It assumes the incoming streams are in sorted order with no duplicates.
pub struct StringMergeStream<'a> {
    states: Vec<StreamState<'a, String>>,
    drained: bool,
}

struct StreamState<'a, T> {
    stream: BoxStream<'a, T>,
    next: Poll<Option<T>>,
}

impl StringMergeStream<'_> {
    #[allow(dead_code)]
    fn new(streams: Vec<BoxStream<'_, String>>) -> StringMergeStream<'_> {
        let states = streams
            .into_iter()
            .map(|s| StreamState {
                stream: s,
                next: Poll::Pending,
            })
            .collect();

        StringMergeStream {
            states,
            drained: false,
        }
    }
}

impl Stream for StringMergeStream<'_> {
    type Item = String;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.drained {
            return Poll::Ready(None);
        }

        let mut one_pending = false;

        for state in &mut self.states {
            if state.next.is_pending() {
                state.next = state.stream.as_mut().poll_next(cx);
                one_pending = one_pending || state.next.is_pending();
            }
        }

        if one_pending {
            return Poll::Pending;
        }

        let mut next_val: Option<String> = None;
        let mut next_pos = 0;

        for (pos, state) in self.states.iter_mut().enumerate() {
            match (&next_val, &state.next) {
                (None, Poll::Ready(Some(ref val))) => {
                    next_val = Some(val.clone());
                    next_pos = pos;
                }
                (Some(next), Poll::Ready(Some(ref val))) => match next.cmp(val) {
                    Ordering::Greater => {
                        next_val = Some(val.clone());
                        next_pos = pos;
                    }
                    Ordering::Equal => {
                        state.next = state.stream.as_mut().poll_next(cx);
                    }
                    _ => (),
                },
                (Some(_), Poll::Ready(None)) => (),
                (None, Poll::Ready(None)) => (),
                _ => unreachable!(),
            }
        }

        if next_val.is_none() {
            self.drained = true;
            return Poll::Ready(None);
        }

        let next_state: &mut StreamState<'_, String> = &mut self.states[next_pos];

        mem::replace(
            &mut next_state.next,
            next_state.stream.as_mut().poll_next(cx),
        )
    }
}

/// ReadMergeStream will do a merge sort of the ReadBatches from multiple partitions. When merging
/// it will ensure that batches are sent through in lexographical order by key. In situations
/// where multiple partitions have batches with the same key, they are merged together in time
/// ascending order. For any given key, multiple read batches can come through.
///
/// It assume that the input streams send batches in key lexographical order and that values are
/// always of the same type for a given key, and that those values are in time sorted order. A
/// stream can have multiple batches with the same key, as long as the values across those batches
/// are in time sorted order (ascending).
pub struct ReadMergeStream<'a> {
    states: Vec<StreamState<'a, ReadBatch>>,
    drained: bool,
}

impl ReadMergeStream<'_> {
    #[allow(dead_code)]
    fn new(streams: Vec<BoxStream<'_, ReadBatch>>) -> ReadMergeStream<'_> {
        let states = streams
            .into_iter()
            .map(|s| StreamState {
                stream: s,
                next: Poll::Pending,
            })
            .collect();

        ReadMergeStream {
            states,
            drained: false,
        }
    }
}

impl Stream for ReadMergeStream<'_> {
    type Item = ReadBatch;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.drained {
            return Poll::Ready(None);
        }

        // ensure that every stream in pending state is called next and return if any are still pending
        let mut one_pending = false;

        for state in &mut self.states {
            if state.next.is_pending() {
                state.next = state.stream.as_mut().poll_next(cx);
                one_pending = one_pending || state.next.is_pending();
            }
        }

        if one_pending {
            return Poll::Pending;
        }

        // find the minimum key for the next batch and keep track of the other batches that have
        // the same key
        let mut next_min_key: Option<String> = None;
        let mut min_time = std::i64::MAX;
        let mut min_pos = 0;
        let mut positions = Vec::with_capacity(self.states.len());

        for (pos, state) in self.states.iter().enumerate() {
            match (&next_min_key, &state.next) {
                (None, Poll::Ready(Some(batch))) => {
                    next_min_key = Some(batch.key.clone());
                    min_pos = pos;
                    let (_, t) = batch.start_stop_times();
                    min_time = t;
                }
                (Some(min_key), Poll::Ready(Some(batch))) => {
                    match min_key.cmp(&batch.key) {
                        Ordering::Greater => {
                            next_min_key = Some(batch.key.clone());
                            min_pos = pos;
                            positions = Vec::with_capacity(self.states.len());
                            let (_, t) = batch.start_stop_times();
                            min_time = t;
                        }
                        Ordering::Equal => {
                            // if this batch has an end time less than the existing min time, make this
                            // the batch that we want to pull out first
                            let (_, t) = batch.start_stop_times();
                            if t < min_time {
                                min_time = t;
                                positions.push(min_pos);
                                min_pos = pos;
                            } else {
                                positions.push(pos);
                            }
                        }
                        _ => (),
                    }
                }
                (Some(_), Poll::Ready(None)) => (),
                (None, Poll::Ready(None)) => (),
                _ => unreachable!(),
            }
        }

        if next_min_key.is_none() {
            self.drained = true;
            return Poll::Ready(None);
        }

        let mut val = mem::replace(&mut self.states[min_pos].next, Poll::Pending);

        if positions.is_empty() {
            return val;
        }

        // pull out all the values with times less than the end time from the val batch
        match &mut val {
            Poll::Ready(Some(batch)) => {
                for pos in positions {
                    if let Poll::Ready(Some(b)) = &mut self.states[pos].next {
                        if batch.append_below_time(b, min_time) {
                            self.states[pos].next = Poll::Pending;
                        }
                    }
                }

                batch.sort_by_time();
            }
            _ => unreachable!(),
        }

        val
    }
}

// TODO: Make a constructor function that fails if given an empty `Vec` of `ReadPoint`s.
#[derive(Debug, PartialEq, Clone)]
pub enum ReadValues {
    I64(Vec<ReadPoint<i64>>),
    F64(Vec<ReadPoint<f64>>),
}

impl ReadValues {
    pub fn is_empty(&self) -> bool {
        match self {
            ReadValues::I64(vals) => vals.is_empty(),
            ReadValues::F64(vals) => vals.is_empty(),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct ReadBatch {
    pub key: String,
    pub values: ReadValues,
}

impl ReadBatch {
    /// Returns the first time and the last time in the batch.
    ///
    /// # Panics
    ///
    /// Will panic if there are no values in the `ReadValues`.
    fn start_stop_times(&self) -> (i64, i64) {
        match &self.values {
            ReadValues::I64(vals) => (vals.first().unwrap().time, vals.last().unwrap().time),
            ReadValues::F64(vals) => (vals.first().unwrap().time, vals.last().unwrap().time),
        }
    }

    fn sort_by_time(&mut self) {
        match &mut self.values {
            ReadValues::I64(vals) => vals.sort_by_key(|v| v.time),
            ReadValues::F64(vals) => vals.sort_by_key(|v| v.time),
        }
    }

    // append_below_time will append all values from other that have a time < than the one passed in.
    // it returns true if other has been cleared of all values
    fn append_below_time(&mut self, other: &mut ReadBatch, t: i64) -> bool {
        match (&mut self.values, &mut other.values) {
            (ReadValues::I64(vals), ReadValues::I64(other_vals)) => {
                let pos = other_vals.iter().position(|val| val.time > t);
                match pos {
                    None => vals.append(other_vals),
                    Some(pos) => vals.extend(other_vals.drain(..pos)),
                }
                other_vals.is_empty()
            }
            (ReadValues::F64(vals), ReadValues::F64(other_vals)) => {
                let pos = other_vals.iter().position(|val| val.time > t);
                match pos {
                    None => vals.append(other_vals),
                    Some(pos) => vals.extend(other_vals.drain(..pos)),
                }
                other_vals.is_empty()
            }
            (_, _) => true, // do nothing here
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{stream, StreamExt};

    #[test]
    fn string_merge_stream() {
        let one = stream::iter(vec!["a".to_string(), "c".to_string()].into_iter());
        let two = stream::iter(vec!["b".to_string(), "c".to_string(), "d".to_string()].into_iter());
        let three =
            stream::iter(vec!["c".to_string(), "e".to_string(), "f".to_string()].into_iter());
        let four = stream::iter(vec![].into_iter());

        let merger =
            StringMergeStream::new(vec![one.boxed(), two.boxed(), three.boxed(), four.boxed()]);

        let stream = futures::executor::block_on_stream(merger);
        let vals: Vec<_> = stream.collect();

        assert_eq!(
            vals,
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
                "e".to_string(),
                "f".to_string()
            ],
        );
    }

    #[test]
    fn read_merge_stream() {
        let one = stream::iter(
            vec![
                ReadBatch {
                    key: "foo".to_string(),
                    values: ReadValues::I64(vec![
                        ReadPoint { time: 3, value: 30 },
                        ReadPoint { time: 4, value: 40 },
                    ]),
                },
                ReadBatch {
                    key: "test".to_string(),
                    values: ReadValues::F64(vec![
                        ReadPoint {
                            time: 1,
                            value: 1.1,
                        },
                        ReadPoint {
                            time: 2,
                            value: 2.2,
                        },
                    ]),
                },
            ]
            .into_iter(),
        );

        let two = stream::iter(
            vec![
                ReadBatch {
                    key: "bar".to_string(),
                    values: ReadValues::F64(vec![
                        ReadPoint {
                            time: 5,
                            value: 5.5,
                        },
                        ReadPoint {
                            time: 6,
                            value: 6.6,
                        },
                    ]),
                },
                ReadBatch {
                    key: "foo".to_string(),
                    values: ReadValues::I64(vec![
                        ReadPoint { time: 1, value: 10 },
                        ReadPoint { time: 2, value: 20 },
                        ReadPoint { time: 6, value: 60 },
                        ReadPoint {
                            time: 11,
                            value: 110,
                        },
                    ]),
                },
            ]
            .into_iter(),
        );

        let three = stream::iter(
            vec![ReadBatch {
                key: "foo".to_string(),
                values: ReadValues::I64(vec![
                    ReadPoint { time: 5, value: 50 },
                    ReadPoint {
                        time: 10,
                        value: 100,
                    },
                ]),
            }]
            .into_iter(),
        );

        let four = stream::iter(vec![].into_iter());

        let merger =
            ReadMergeStream::new(vec![one.boxed(), two.boxed(), three.boxed(), four.boxed()]);
        let stream = futures::executor::block_on_stream(merger);
        let vals: Vec<_> = stream.collect();

        assert_eq!(
            vals,
            vec![
                ReadBatch {
                    key: "bar".to_string(),
                    values: ReadValues::F64(vec![
                        ReadPoint {
                            time: 5,
                            value: 5.5
                        },
                        ReadPoint {
                            time: 6,
                            value: 6.6
                        },
                    ]),
                },
                ReadBatch {
                    key: "foo".to_string(),
                    values: ReadValues::I64(vec![
                        ReadPoint { time: 1, value: 10 },
                        ReadPoint { time: 2, value: 20 },
                        ReadPoint { time: 3, value: 30 },
                        ReadPoint { time: 4, value: 40 },
                    ]),
                },
                ReadBatch {
                    key: "foo".to_string(),
                    values: ReadValues::I64(vec![
                        ReadPoint { time: 5, value: 50 },
                        ReadPoint { time: 6, value: 60 },
                        ReadPoint {
                            time: 10,
                            value: 100
                        },
                    ]),
                },
                ReadBatch {
                    key: "foo".to_string(),
                    values: ReadValues::I64(vec![ReadPoint {
                        time: 11,
                        value: 110
                    },]),
                },
                ReadBatch {
                    key: "test".to_string(),
                    values: ReadValues::F64(vec![
                        ReadPoint {
                            time: 1,
                            value: 1.1
                        },
                        ReadPoint {
                            time: 2,
                            value: 2.2
                        }
                    ]),
                },
            ],
        )
    }

    use futures::executor;
    use proptest::prelude::*;
    use std::task::Poll;

    /// Given a vector of optional values, put all the `Some` values in sorted order, leaving the
    /// `None` values interspersed as they are.
    fn sort_present_values<T: Ord>(d: &mut Vec<Option<T>>) {
        let mut values = vec![];
        let mut indices = vec![];

        for (i, v) in d.iter_mut().enumerate() {
            if let Some(v) = v.take() {
                values.push(v);
                indices.push(i);
            }
        }

        values.sort();

        for (v, i) in values.into_iter().zip(indices) {
            d[i] = Some(v);
        }
    }

    /// Adds an arbitrary number of `Poll::Pending` values throughout the `Poll::Ready(Some(T))`
    /// values returned by the stream.
    fn arb_proto_stream<T: Arbitrary + Ord + Clone>() -> impl Strategy<Value = ProtoStream<T>> {
        // Generate a vector of optional values
        any::<Vec<Option<T>>>().prop_map(|mut v| {
            sort_present_values(&mut v);

            // `original` will contain the sorted present values only
            let original = v.clone().into_iter().flatten().collect();
            let poll_values = v
                .into_iter()
                .map(|v| match v {
                    // Convert optional values to `Poll` values
                    Some(v) => Poll::Ready(v),
                    None => Poll::Pending,
                })
                .collect();
            ProtoStream {
                original,
                poll_values,
            }
        })
    }

    #[derive(Debug, Clone)]
    struct ProtoStream<T> {
        original: Vec<T>,
        poll_values: Vec<Poll<T>>,
    }

    impl<T: 'static> ProtoStream<T> {
        fn to_stream(self) -> (Vec<T>, impl Stream<Item = T>) {
            let Self {
                original,
                mut poll_values,
            } = self;

            // Since we pop off the poll values for implementation
            // simplicity, we reverse them first so the original
            // values line up.
            poll_values.reverse();

            let mut exhausted = false;

            (
                original,
                stream::poll_fn(move |ctx| {
                    // Always schedule a wakeup as nothing else will!
                    ctx.waker().wake_by_ref();

                    match poll_values.pop() {
                        Some(v) => v.map(Some),
                        None if !exhausted => {
                            // Always return a `Poll::Ready(None)` when all data has been returned.
                            exhausted = true;
                            Poll::Ready(None)
                        }
                        // A property of `ProtoStream`s is they're never polled after returning
                        // `Poll::Ready(None)`.
                        None => panic!("stream polled after it was exhausted"),
                    }
                }),
            )
        }
    }

    proptest! {
        #[test]
        fn test_add(a in arb_proto_stream::<i32>()) {
            let (values, stream) = a.to_stream();
            let stream_vals: Vec<_> = executor::block_on_stream(stream).collect();
            prop_assert_eq!(&values, &stream_vals);
            prop_assert!(values.is_empty());
        }
    }
}
