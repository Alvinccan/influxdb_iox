#![allow(dead_code)]

use crate::delorean::{Node, Predicate, TimestampRange};
use crate::line_parser::{ParseError, Point, PointType};
use crate::storage::partitioned_store::{ReadBatch, ReadValues};
use crate::storage::predicate::{Evaluate, EvaluateVisitor};
use crate::storage::series_store::ReadPoint;
use crate::storage::{SeriesDataType, StorageError};

use croaring::Treemap;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap};

/// memdb implements an in memory database for the Partition trait. It currently assumes that
/// data arrives in time ascending order per series. It has no limits on the number of series
/// or the amount of data per series. It is up to the higher level database to decide when to
/// stop writing into a given MemDB.

// TODO: return errors if trying to insert data out of order in an individual series

#[derive(Default)]
pub struct MemDB {
    series_data: SeriesData,
    series_map: SeriesMap,
}

#[derive(Default)]
struct SeriesData {
    current_size: usize,
    i64_series: HashMap<u64, SeriesBuffer<i64>>,
    f64_series: HashMap<u64, SeriesBuffer<f64>>,
}

struct SeriesBuffer<T: Clone> {
    values: Vec<ReadPoint<T>>,
}

impl<T: Clone> SeriesBuffer<T> {
    fn read(&self, range: &TimestampRange) -> Vec<ReadPoint<T>> {
        let start = match self.values
            .iter()
            .position(|val| val.time >= range.start) {
            Some(pos) => pos,
            None => return vec![],
        };

        let stop = self.values
            .iter()
            .position(|val| val.time >= range.end);
        let stop = stop.unwrap_or_else(|| self.values.len());

        self.values[start..stop].to_vec()
    }
}

trait StoreInSeriesData {
    fn write(&self, series_data: &mut SeriesData);
}

impl StoreInSeriesData for PointType {
    fn write(&self, series_data: &mut SeriesData) {
        match self {
            PointType::I64(inner) => inner.write(series_data),
            PointType::F64(inner) => inner.write(series_data),
        }
    }
}

impl StoreInSeriesData for Point<i64> {
    fn write(&self, series_data: &mut SeriesData) {
        let point: ReadPoint<_> = self.into();
        series_data.current_size += std::mem::size_of::<ReadPoint<i64>>();

        match series_data.i64_series.get_mut(&self.series_id.unwrap()) {
            Some(buff) => buff.values.push(point),
            None => {
                let buff = SeriesBuffer {
                    values: vec![point],
                };
                series_data.i64_series.insert(self.series_id.unwrap(), buff);
            }
        }
    }
}

impl StoreInSeriesData for Point<f64> {
    fn write(&self, series_data: &mut SeriesData) {
        let point: ReadPoint<_> = self.into();
        series_data.current_size += std::mem::size_of::<Point<f64>>();

        match series_data.f64_series.get_mut(&self.series_id.unwrap()) {
            Some(buff) => buff.values.push(point),
            None => {
                let buff = SeriesBuffer {
                    values: vec![point],
                };
                series_data.f64_series.insert(self.series_id.unwrap(), buff);
            }
        }
    }
}

#[derive(Default)]
struct SeriesMap {
    current_size: usize,
    last_id: u64,
    series_key_to_id: HashMap<String, u64>,
    series_id_to_key_and_type: HashMap<u64, (String, SeriesDataType)>,
    tag_keys: BTreeMap<String, BTreeMap<String, bool>>,
    posting_list: HashMap<Vec<u8>, Treemap>,
}

impl SeriesMap {
    /// The number of copies of the key this map contains. This is
    /// used to provide a rough estimate of the memory size.
    ///
    /// It occurs:
    ///
    /// 1. in the map to ID
    /// 2. in the ID to map
    const SERIES_KEY_COPIES: usize = 2;
    /// The number of bytes the different copies of the series ID in
    /// this map represents. This is used to provide a rough estimate
    /// of the memory size.
    const SERIES_ID_BYTES: usize = 24;

    fn insert_series(&mut self, point: &mut PointType) -> Result<(), ParseError> {
        if let Some(id) = self.series_key_to_id.get(point.series()) {
            point.set_series_id(*id);
            return Ok(());
        }

        // insert the series id
        self.last_id += 1;
        point.set_series_id(self.last_id);
        self.series_key_to_id
            .insert(point.series().clone(), self.last_id);

        let series_type = match point {
            PointType::I64(_) => SeriesDataType::I64,
            PointType::F64(_) => SeriesDataType::F64,
        };
        self.series_id_to_key_and_type
            .insert(self.last_id, (point.series().clone(), series_type));

        // update the estimated size of the map.
        self.current_size +=
            point.series().len() * SeriesMap::SERIES_KEY_COPIES + SeriesMap::SERIES_ID_BYTES;

        for pair in point.index_pairs()? {
            // insert this id into the posting list
            let list_key = list_key(&pair.key, &pair.value);

            // update estimated size for the index pairs
            self.current_size += list_key.len() + pair.key.len() + pair.value.len();

            let posting_list = self
                .posting_list
                .entry(list_key)
                .or_insert_with(Treemap::create);
            posting_list.add(self.last_id);

            // insert the tag key value mapping
            let tag_values = self.tag_keys.entry(pair.key).or_insert_with(BTreeMap::new);
            tag_values.insert(pair.value, true);
        }

        Ok(())
    }

    fn posting_list_for_key_value(&self, key: &str, value: &str) -> Treemap {
        let list_key = list_key(key, value);
        match self.posting_list.get(&list_key) {
            Some(m) => m.clone(),
            None => Treemap::create(),
        }
    }
}

fn list_key(key: &str, value: &str) -> Vec<u8> {
    let mut list_key = key.as_bytes().to_vec();
    list_key.push(0 as u8);
    list_key.append(&mut value.as_bytes().to_vec());
    list_key
}

impl MemDB {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn size(&self) -> usize {
        self.series_data.current_size + self.series_map.current_size
    }

    fn write(&mut self, points: &mut [PointType]) -> Result<(), StorageError> {
        for p in points {
            self.series_map.insert_series(p).map_err(|e| StorageError {
                description: format!("error parsing line protocol metadata {}", e),
            })?;
            p.write(&mut self.series_data);
        }

        Ok(())
    }

    fn get_tag_keys(
        &self,
        _range: &TimestampRange,
        _predicate: &Predicate,
    ) -> Result<BoxStream<'_, String>, StorageError> {
        let keys = self.series_map.tag_keys.keys().cloned();
        Ok(stream::iter(keys).boxed())
    }

    fn get_tag_values(
        &self,
        tag_key: &str,
        _range: &TimestampRange,
        _predicate: &Predicate,
    ) -> Result<BoxStream<'_, String>, StorageError> {
        match self.series_map.tag_keys.get(tag_key) {
            Some(values) => {
                let values = values.keys().cloned();
                Ok(stream::iter(values).boxed())
            }
            None => Ok(stream::empty().boxed()),
        }
    }

    fn read(
        &self,
        _batch_size: usize,
        predicate: &Predicate,
        range: &TimestampRange,
    ) -> Result<BoxStream<'_, ReadBatch>, StorageError> {
        let root = match &predicate.root {
            Some(r) => r,
            None => {
                return Err(StorageError {
                    description: "expected root node to evaluate".to_string(),
                })
            }
        };

        let map = evaluate_node(&self.series_map, &root)?;
        let mut read_batches = Vec::with_capacity(map.cardinality() as usize);

        for id in map.iter() {
            let (key, series_type) = self.series_map.series_id_to_key_and_type.get(&id).unwrap();

            let values = match series_type {
                SeriesDataType::I64 => {
                    let buff = self.series_data.i64_series.get(&id).unwrap();
                    ReadValues::I64(buff.read(range))
                }
                SeriesDataType::F64 => {
                    let buff = self.series_data.f64_series.get(&id).unwrap();
                    ReadValues::F64(buff.read(range))
                }
            };

            // TODO: Encode in the type system that `ReadBatch`es will never be created with an
            // empty vector, as we're doing here.
            if values.is_empty() {
                continue;
            }

            let batch = ReadBatch {
                key: key.to_string(),
                values,
            };

            read_batches.push(batch);
        }

        Ok(stream::iter(read_batches.into_iter()).boxed())
    }
}

fn evaluate_node(series_map: &SeriesMap, n: &Node) -> Result<Treemap, StorageError> {
    struct Visitor<'a>(&'a SeriesMap);

    impl EvaluateVisitor for Visitor<'_> {
        fn equal(&mut self, left: &str, right: &str) -> Result<Treemap, StorageError> {
            Ok(self.0.posting_list_for_key_value(left, right))
        }
    }

    Evaluate::evaluate(Visitor(series_map), n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::predicate::parse_predicate;

    #[test]
    fn write_and_read_tag_keys() {
        let memdb = setup_db();
        let tag_keys = memdb
            .get_tag_keys(
                &TimestampRange { start: 0, end: 0 },
                &Predicate { root: None },
            )
            .unwrap();
        let tag_keys: Vec<_> = futures::executor::block_on_stream(tag_keys).collect();

        assert_eq!(tag_keys, vec!["_f", "_m", "host", "region"]);
    }

    #[test]
    fn write_and_read_tag_values() {
        let memdb = setup_db();
        let tag_values = memdb
            .get_tag_values(
                "host",
                &TimestampRange { start: 0, end: 0 },
                &Predicate { root: None },
            )
            .unwrap();
        let tag_values: Vec<_> = futures::executor::block_on_stream(tag_values).collect();
        assert_eq!(tag_values, vec!["a", "b"]);
    }

    #[test]
    fn write_and_check_size() {
        let memdb = setup_db();
        assert_eq!(memdb.size(), 704);
    }

    #[test]
    fn write_and_get_measurement_series() {
        let memdb = setup_db();
        let pred = parse_predicate(r#"_m = "cpu""#).unwrap();
        let batches = memdb
            .read(10, &pred, &TimestampRange { start: 0, end: 5 })
            .unwrap();
        let batches: Vec<_> = futures::executor::block_on_stream(batches).collect();

        assert_eq!(
            batches,
            vec![
                ReadBatch {
                    key: "cpu,host=b,region=west\tusage_system".to_string(),
                    values: ReadValues::I64(vec![
                        ReadPoint { time: 0, value: 1 },
                        ReadPoint { time: 1, value: 2 },
                    ]),
                },
                ReadBatch {
                    key: "cpu,host=a,region=west\tusage_system".to_string(),
                    values: ReadValues::I64(vec![ReadPoint { time: 0, value: 1 }]),
                },
                ReadBatch {
                    key: "cpu,host=a,region=west\tusage_user".to_string(),
                    values: ReadValues::I64(vec![ReadPoint { time: 0, value: 1 }]),
                },
            ],
        );
    }

    #[test]
    fn write_and_get_tag_match_series() {
        let memdb = setup_db();
        let pred = parse_predicate(r#"host = "a""#).unwrap();
        let batches = memdb
            .read(10, &pred, &TimestampRange { start: 0, end: 5 })
            .unwrap();
        let batches: Vec<_> = futures::executor::block_on_stream(batches).collect();
        assert_eq!(
            batches,
            vec![
                ReadBatch {
                    key: "cpu,host=a,region=west\tusage_system".to_string(),
                    values: ReadValues::I64(vec![ReadPoint { time: 0, value: 1 }]),
                },
                ReadBatch {
                    key: "cpu,host=a,region=west\tusage_user".to_string(),
                    values: ReadValues::I64(vec![ReadPoint { time: 0, value: 1 }]),
                },
            ]
        );
    }

    #[test]
    fn write_and_measurement_and_tag_match_series() {
        let memdb = setup_db();
        let pred = parse_predicate(r#"_m = "cpu" and host = "b""#).unwrap();
        let batches = memdb
            .read(10, &pred, &TimestampRange { start: 0, end: 5 })
            .unwrap();
        let batches: Vec<_> = futures::executor::block_on_stream(batches).collect();
        assert_eq!(
            batches,
            vec![ReadBatch {
                key: "cpu,host=b,region=west\tusage_system".to_string(),
                values: ReadValues::I64(vec![
                    ReadPoint { time: 0, value: 1 },
                    ReadPoint { time: 1, value: 2 },
                ]),
            },]
        );
    }

    #[test]
    fn write_and_measurement_or_tag_match() {
        let memdb = setup_db();
        let pred = parse_predicate(r#"host = "a" OR _m = "mem""#).unwrap();
        let batches = memdb
            .read(10, &pred, &TimestampRange { start: 0, end: 5 })
            .unwrap();
        let batches: Vec<_> = futures::executor::block_on_stream(batches).collect();
        assert_eq!(
            batches,
            vec![
                ReadBatch {
                    key: "cpu,host=a,region=west\tusage_system".to_string(),
                    values: ReadValues::I64(vec![ReadPoint { time: 0, value: 1 },]),
                },
                ReadBatch {
                    key: "cpu,host=a,region=west\tusage_user".to_string(),
                    values: ReadValues::I64(vec![ReadPoint { time: 0, value: 1 },]),
                },
                ReadBatch {
                    key: "mem,host=b,region=west\tfree".to_string(),
                    values: ReadValues::I64(vec![ReadPoint { time: 0, value: 1 },]),
                },
            ]
        );
    }

    fn setup_db() -> MemDB {
        let p1 = PointType::new_i64("cpu,host=b,region=west\tusage_system".to_string(), 1, 0);
        let p2 = PointType::new_i64("cpu,host=a,region=west\tusage_system".to_string(), 1, 0);
        let p3 = PointType::new_i64("cpu,host=a,region=west\tusage_user".to_string(), 1, 0);
        let p4 = PointType::new_i64("mem,host=b,region=west\tfree".to_string(), 1, 0);
        let p5 = PointType::new_i64("cpu,host=b,region=west\tusage_system".to_string(), 2, 1);

        let mut points = vec![p1, p2, p3, p4, p5];

        let mut memdb = MemDB::new();
        memdb.write(&mut points).unwrap();
        memdb
    }

    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    struct SeriesBufferReadData<T: Clone> {
        range: TimestampRange,
        before: Vec<ReadPoint<T>>,
        during: Vec<ReadPoint<T>>,
        after: Vec<ReadPoint<T>>,
    }

    impl<T: Clone> SeriesBufferReadData<T> {
        fn series_buffer(&self) -> SeriesBuffer<T> {
            let mut values: Vec<_> = self
                .before
                .iter()
                .cloned()
                .chain(self.during.iter().cloned())
                .chain(self.after.iter().cloned())
                .collect();
            values.sort_by_key(|v| v.time);
            SeriesBuffer { values }
        }
    }

    fn arb_read_point_sorted_vec<T: Arbitrary + Clone>(start: Option<i64>, end: Option<i64>) -> impl Strategy<Value = Vec<ReadPoint<T>>> {
        let start = start.unwrap_or(i64::min_value());
        let end = end.unwrap_or(i64::max_value());

        prop::collection::vec((any::<T>(), start..end), 0..10).prop_map(|mut v| {
            v.sort_by_key(|tup| tup.1);
            v.into_iter().map(|(value, time)| ReadPoint { value, time }).collect()
        })
    }

    fn arb_series_buffer<T: Arbitrary + Clone + std::fmt::Debug>() -> impl Strategy<Value = SeriesBufferReadData<T>> {
        (0..i64::max_value(), 0..i64::max_value()).prop_flat_map(|(a_time, b_time)| {
            let (start, end) = if a_time < b_time {
                (a_time, b_time)
            } else {
                (b_time, a_time)
            };

            let range = TimestampRange { start, end };
            let before = arb_read_point_sorted_vec::<T>(None, Some(start));
            let during = arb_read_point_sorted_vec::<T>(Some(start), Some(end));
            let after = arb_read_point_sorted_vec::<T>(Some(end), None);

            (Just(range), before, during, after)
            }).prop_map(|(range, before, during, after)| SeriesBufferReadData { range, before, during, after })

    }

    proptest! {
        #[test]
        fn test_series_buffer_read(a in arb_series_buffer::<i64>()) {
            let series_buffer = a.series_buffer();

            prop_assert_eq!(series_buffer.read(&a.range), a.during);
        }
    }
}
