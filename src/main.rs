use arrow::record_batch::RecordBatch;
use arrow::csv::reader;
use arrow::ipc::writer::*;
use std::fs::{File, read};
use arrow::util::pretty;
use std::path;
use std::result::*;
use std::io::Cursor;
use arrow::ipc::writer::FileWriter;
use arrow::csv;
use std::io;
use std::time;
use arrow::util::pretty::pretty_format_batches;
use arrow::array::*;
use arrow::datatypes::{Field, Schema, DataType};
use arrow::compute::kernels::filter;
use datafusion::logicalplan::ScalarValue;
use datafusion::execution::physical_plan::PhysicalExpr;
use datafusion::execution::physical_plan::hash_aggregate::HashAggregateExec;
use std::sync::Arc;
use arrow::error::ArrowError;
use std::collections::{BTreeMap, BTreeSet};
use crate::Error::Unexpected;
use arrow::ipc::Utf8Builder;
use std::iter::repeat;

pub struct ProduceContext {}

pub struct ExecutionContext {}

pub enum Error {
    IOError(io::Error),
    ArrowError(arrow::error::ArrowError),
    Unexpected,
}

impl From<arrow::error::ArrowError> for Error {
    fn from(err: ArrowError) -> Self {
        Error::ArrowError(err)
    }
}

pub type ProduceFn<'a> = &'a mut dyn FnMut(&ProduceContext, RecordBatch) -> Result<(), Error>;
pub type MetaSendFn<'a> = &'a mut dyn FnMut(&ProduceContext, MetadataMessage) -> Result<(), Error>;

enum MetadataMessage {
    EndOfStream,
}

pub trait Node {
    fn schema(&self) -> Result<Arc<Schema>, Error>;
    fn run(&self, ctx: &ExecutionContext, produce: ProduceFn, meta_send: MetaSendFn) -> Result<(), Error>;
}

fn record_print(ctx: &ProduceContext, batch: RecordBatch) -> Result<(), Error> {
    println!("{}", batch.num_rows());
    println!("{}", pretty_format_batches(&[batch]).unwrap());
    Ok(())
}

fn noop_meta_send(ctx: &ProduceContext, msg: MetadataMessage) -> Result<(), Error> {
    Ok(())
}

pub struct CSVSource<'a> {
    path: &'a str
}

impl<'a> CSVSource<'a> {
    fn new(path: &'a str) -> CSVSource<'a> {
        CSVSource { path }
    }
}

impl<'a> Node for CSVSource<'a> {
    fn schema(&self) -> Result<Arc<Schema>, Error> {
        let file = File::open(self.path).unwrap();
        let r = csv::ReaderBuilder::new()
            .has_header(true)
            .infer_schema(Some(10))
            .with_batch_size(8192 * 2)
            .build(file).unwrap();
        let mut fields = r.schema().fields().clone();
        fields.push(Field::new("retraction", DataType::Boolean, false));

        Ok(Arc::new(Schema::new(fields)))
    }

    fn run(&self, ctx: &ExecutionContext, produce: ProduceFn, meta_send: MetaSendFn) -> Result<(), Error> {
        let file = File::open(self.path).unwrap();
        let mut r = csv::ReaderBuilder::new()
            .has_header(true)
            .infer_schema(Some(10))
            .with_batch_size(8192)
            .build(file).unwrap();
        let mut retraction_array_builder = BooleanBuilder::new(8192);
        for i in 0..8192 {
            retraction_array_builder.append_value(false);
        }
        let retraction_array = Arc::new(retraction_array_builder.finish());
        let schema = self.schema()?;
        loop {
            let maybe_rec = r.next().unwrap();
            match maybe_rec {
                None => break,
                Some(rec) => {
                    let mut columns: Vec<ArrayRef> = rec.columns().iter().cloned().collect();
                    if columns[0].len() == 8192 {
                        columns.push(retraction_array.clone() as ArrayRef)
                    } else {
                        let mut retraction_array_builder = BooleanBuilder::new(8192);
                        for i in 0..columns[0].len() {
                            retraction_array_builder.append_value(false);
                        }
                        let retraction_array = Arc::new(retraction_array_builder.finish());
                        columns.push(retraction_array as ArrayRef)
                    }
                    produce(&ProduceContext {}, RecordBatch::try_new(schema.clone(), columns).unwrap())
                }
            };
        }
        Ok(())
    }
}

pub struct Projection<'a, 'b> {
    fields: &'b [&'a str],
    source: Box<dyn Node>,
}

impl<'a, 'b> Projection<'a, 'b> {
    fn new(fields: &'b [&'a str], source: Box<dyn Node>) -> Projection<'a, 'b> {
        Projection { fields, source }
    }

    fn schema_from_source_schema(&self, source_schema: Arc<Schema>) -> Result<Arc<Schema>, Error> {
        let new_schema_fields: Vec<Field> = self.fields
            .into_iter()
            .map(|&field| source_schema.index_of(field).unwrap())
            .map(|i| source_schema.field(i).clone())
            .collect();
        Ok(Arc::new(Schema::new(new_schema_fields)))
    }
}

impl<'a, 'b> Node for Projection<'a, 'b> {
    fn schema(&self) -> Result<Arc<Schema>, Error> {
        let source_schema = self.source.schema()?;
        self.schema_from_source_schema(source_schema)
    }

    fn run(&self, ctx: &ExecutionContext, produce: ProduceFn, meta_send: MetaSendFn) -> Result<(), Error> {
        let source_schema = self.source.schema()?;
        let new_schema = self.schema_from_source_schema(source_schema.clone())?;

        let indices: Vec<usize> = self.fields.into_iter()
            .map(|&field| source_schema.index_of(field).unwrap())
            .collect();

        self.source.run(ctx, &mut |ctx, batch| {
            let new_columns: Vec<ArrayRef> = (&indices).into_iter()
                .map(|&i| batch.column(i).clone())
                .collect();

            let new_batch = RecordBatch::try_new(
                new_schema.clone(),
                new_columns,
            ).unwrap();

            produce(ctx, new_batch)?;
            Ok(())
        }, &mut noop_meta_send);
        Ok(())
    }
}

pub struct Filter<'a> {
    field: &'a str,
    source: Box<dyn Node>,
}

impl<'a> Filter<'a> {
    fn new(field: &'a str, source: Box<dyn Node>) -> Filter<'a> {
        Filter { field, source }
    }
}

impl<'a> Node for Filter<'a> {
    fn schema(&self) -> Result<Arc<Schema>, Error> {
        self.source.schema()
    }

    fn run(&self, ctx: &ExecutionContext, produce: ProduceFn, meta_send: MetaSendFn) -> Result<(), Error> {
        let source_schema = self.source.schema()?;
        let index_of_field = source_schema.index_of(self.field)?;

        self.source.run(ctx, &mut |ctx, batch| {
            let predicate_column = batch.column(index_of_field).as_any().downcast_ref::<BooleanArray>().unwrap();
            let new_columns = batch
                .columns()
                .into_iter()
                .map(|array_ref| { filter::filter(array_ref.as_ref(), predicate_column).unwrap() })
                .collect();
            let new_batch = RecordBatch::try_new(
                source_schema.clone(),
                new_columns,
            ).unwrap();
            produce(ctx, batch);
            Ok(())
        }, &mut noop_meta_send);
        Ok(())
    }
}

pub trait Aggregate {
    fn output_type(&self, input_schema: &DataType) -> Result<DataType, Error>;
    fn create_accumulator(&self) -> Box<dyn Accumulator>;
}

pub trait Accumulator: std::fmt::Debug {
    fn add(&mut self, value: ScalarValue, retract: ScalarValue) -> bool;
    fn trigger(&self) -> ScalarValue;
}

struct Sum {}

impl Aggregate for Sum {
    fn output_type(&self, input_type: &DataType) -> Result<DataType, Error> {
        Ok(DataType::Int64)
    }

    fn create_accumulator(&self) -> Box<dyn Accumulator> {
        Box::new(SumAccumulator { sum: 0, count: 0 })
    }
}

#[derive(Debug)]
struct SumAccumulator {
    sum: i64,
    count: i64,
}

impl Accumulator for SumAccumulator {
    fn add(&mut self, value: ScalarValue, retract: ScalarValue) -> bool {
        let is_retraction = match retract {
            ScalarValue::Boolean(x) => x,
            _ => panic!("retraction shall be boolean"),
        };
        let multiplier = if !is_retraction { 1 } else { -1 };
        if is_retraction {
            self.count -= 1;
        } else {
            self.count += 1;
        }
        match value {
            ScalarValue::Int64(x) => {
                self.sum += x * multiplier;
            }
            _ => panic!("bad aggregate argument")
        }
        self.count != 0
    }

    fn trigger(&self) -> ScalarValue {
        return ScalarValue::Int64(self.sum);
    }
}

pub struct GroupBy {
    key: Vec<String>,
    aggregated_fields: Vec<String>,
    aggregates: Vec<Box<dyn Aggregate>>,
    output_names: Vec<String>,
    source: Box<dyn Node>,
}

impl GroupBy {
    fn new(
        key: Vec<String>,
        aggregated_fields: Vec<String>,
        aggregates: Vec<Box<dyn Aggregate>>,
        output_names: Vec<String>,
        source: Box<dyn Node>,
    ) -> GroupBy {
        return GroupBy {
            key,
            aggregated_fields,
            aggregates,
            output_names,
            source,
        };
    }
}

impl Node for GroupBy {
    fn schema(&self) -> Result<Arc<Schema>, Error> {
        let source_schema = self.source.schema()?;
        let mut key_fields: Vec<Field> = self.key
            .iter()
            .map(|key_field| { source_schema.index_of(key_field).unwrap() })
            .map(|i| source_schema.field(i))
            .cloned()
            .collect();

        let aggregated_field_types: Vec<DataType> = self.aggregated_fields
            .iter()
            .map(|field| { source_schema.index_of(field.as_str()).unwrap() })
            .enumerate()
            .map(|(i, column_index)| {
                self.aggregates[i].output_type(source_schema.field(column_index).data_type())
            })
            .map(|t_res| match t_res {
                Ok(t) => t,
                Err(e) => panic!(e)
            })
            .collect();

        let mut new_fields: Vec<Field> = aggregated_field_types
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, t)| {
                Field::new(self.output_names[i].as_str(), t, false)
            })
            .collect();

        key_fields.append(&mut new_fields);
        key_fields.push(Field::new("retraction", DataType::Boolean, false));
        Ok(Arc::new(Schema::new(key_fields)))
    }

    fn run(&self, ctx: &ExecutionContext, produce: ProduceFn, meta_send: MetaSendFn) -> Result<(), Error> {
        let source_schema = self.source.schema()?;
        let key_indices: Vec<usize> = self.key
            .iter()
            .map(|key_field| { source_schema.index_of(key_field).unwrap() })
            .collect();
        let aggregated_field_indices: Vec<usize> = self.aggregated_fields
            .iter()
            .map(|field| { source_schema.index_of(field.as_str()).unwrap() })
            .collect();

        let mut accumulators_map: BTreeMap<Vec<GroupByScalar>, Vec<Box<dyn Accumulator>>> = BTreeMap::new();
        let mut last_triggered_values: BTreeMap<Vec<GroupByScalar>, Vec<ScalarValue>> = BTreeMap::new();

        let key_types: Vec<DataType> = match self.source.schema() {
            Ok(schema) => self.key
                .iter()
                .map(|field| schema.field_with_name(field).unwrap().data_type())
                .cloned()
                .collect(),
            _ => panic!("aaa"),
        };
        let mut trigger: Box<dyn Trigger> = Box::new(CountingTrigger::new(key_types, 100));

        self.source.run(ctx, &mut |ctx, batch| {
            let key_columns: Vec<ArrayRef> = key_indices
                .iter()
                .map(|&i| { batch.column(i) })
                .cloned()
                .collect();
            let aggregated_columns: Vec<ArrayRef> = aggregated_field_indices
                .iter()
                .map(|&i| { batch.column(i) })
                .cloned()
                .collect();

            let mut key_vec: Vec<GroupByScalar> = Vec::with_capacity(key_columns.len());
            for i in 0..key_columns.len() {
                key_vec.push(GroupByScalar::Int64(0))
            }

            for row in 0..aggregated_columns[0].len() {
                create_key(key_columns.as_slice(), row, &mut key_vec);

                let accumulators = accumulators_map
                    .entry(key_vec.clone())
                    .or_insert(self.aggregates
                        .iter()
                        .map(|aggr| { aggr.create_accumulator() })
                        .collect());
                accumulators
                    .iter_mut()
                    .enumerate()
                    .for_each(|(i, acc)| {
                        acc.add(ScalarValue::Int64(aggregated_columns[i].as_any().downcast_ref::<Int64Array>().unwrap().value(i)), ScalarValue::Boolean(false));
                    })
            }

            trigger.keys_received(key_columns);

            // Check if we can trigger something
            let mut output_columns = trigger.poll();
            if output_columns[0].len() == 0 {
                return Ok(());
            }
            let output_schema = self.schema()?;

            let mut retraction_columns = Vec::with_capacity(self.output_names.len());

            // Push retraction keys
            for key_index in 0..self.key.len() {
                match output_schema.fields()[key_index].data_type() {
                    DataType::Utf8 => {
                        let mut array = StringBuilder::new(output_columns[0].len());
                        for row in 0..output_columns[0].len() {
                            create_key(output_columns.as_slice(), row, &mut key_vec);

                            if !last_triggered_values.contains_key(&key_vec) {
                                continue
                            }

                            match &key_vec[key_index] {
                                GroupByScalar::Utf8(text) => array.append_value(text.as_str()).unwrap(),
                                _ => panic!("bug: key doesn't match schema"),
                                // TODO: Maybe use as_any -> downcast?
                            }
                        }
                        retraction_columns.push(Arc::new(array.finish()) as ArrayRef);
                    }
                    DataType::Int64 => {
                        let mut array = Int64Builder::new(output_columns[0].len());
                        for row in 0..output_columns[0].len() {
                            create_key(output_columns.as_slice(), row, &mut key_vec);

                            if !last_triggered_values.contains_key(&key_vec) {
                                continue
                            }

                            match key_vec[key_index] {
                                GroupByScalar::Int64(n) => array.append_value(n).unwrap(),
                                _ => panic!("bug: key doesn't match schema"),
                                // TODO: Maybe use as_any -> downcast?
                            }
                        }
                        retraction_columns.push(Arc::new(array.finish()) as ArrayRef);
                    }
                    _ => unimplemented!(),
                }
            }

            // Push retractions
            for aggregate_index in 0..self.aggregates.len() {
                match output_schema.fields()[key_indices.len() + aggregate_index].data_type() {
                    DataType::Int64 => {
                        let mut array = Int64Builder::new(output_columns[0].len());
                        for row in 0..output_columns[0].len() {
                            create_key(output_columns.as_slice(), row, &mut key_vec);

                            let last_triggered = last_triggered_values.get(&key_vec);
                            let last_triggered_row = match last_triggered {
                                None => continue,
                                Some(v) => v,
                            };

                            match last_triggered_row[aggregate_index] {
                                ScalarValue::Int64(n) => array.append_value(n).unwrap(),
                                _ => panic!("bug: key doesn't match schema"),
                                // TODO: Maybe use as_any -> downcast?
                            }
                        }
                        retraction_columns.push(Arc::new(array.finish()) as ArrayRef);
                    }
                    _ => unimplemented!(),
                }
            }
            // Remove those values
            for row in 0..output_columns[0].len() {
                create_key(output_columns.as_slice(), row, &mut key_vec);
                last_triggered_values.remove(&key_vec);
            }
            // Build retraction array
            let mut retraction_array_builder = BooleanBuilder::new(retraction_columns[0].len() + output_columns[0].len());
            for i in 0..retraction_columns[0].len() {
                retraction_array_builder.append_value(true);
            }
            for i in 0..output_columns[0].len() {
                retraction_array_builder.append_value(false);
            }
            let retraction_array = Arc::new(retraction_array_builder.finish());

            // Push new values
            for aggregate_index in 0..self.aggregates.len() {
                match output_schema.fields()[key_indices.len() + aggregate_index].data_type() {
                    DataType::Int64 => {
                        let mut array = Int64Builder::new(output_columns[0].len());

                        for row in 0..output_columns[0].len() {
                            create_key(output_columns.as_slice(), row, &mut key_vec);
                            // TODO: this key may not exist because of retractions.
                            let row_accumulators = accumulators_map.get(&key_vec).unwrap();

                            match row_accumulators[aggregate_index].trigger() {
                                ScalarValue::Int64(n) => array.append_value(n).unwrap(),
                                _ => panic!("bug: key doesn't match schema"),
                                // TODO: Maybe use as_any -> downcast?
                            }

                            let mut last_values_vec = last_triggered_values
                                .entry(key_vec.clone())
                                .or_default();
                            last_values_vec.push(row_accumulators[aggregate_index].trigger());
                        }
                        output_columns.push(Arc::new(array.finish()) as ArrayRef);
                    }
                    _ => unimplemented!(),
                }
            }

            // Combine key columns
            for col_index in 0..output_columns.len() {
                match output_schema.fields()[col_index].data_type() {
                    DataType::Utf8 => {
                        let mut array = StringBuilder::new(retraction_columns[0].len() + output_columns[0].len());
                        array.append_data(&[retraction_columns[col_index].data(), output_columns[col_index].data()]);
                        output_columns[col_index] = Arc::new(array.finish()) as ArrayRef;
                    }
                    DataType::Int64 => {
                        let mut array = Int64Builder::new(retraction_columns[0].len() + output_columns[0].len());
                        array.append_data(&[retraction_columns[col_index].data(), output_columns[col_index].data()]);
                        output_columns[col_index] = Arc::new(array.finish()) as ArrayRef;
                    }
                    _ => unimplemented!(),
                }
            }

            // Add retraction array
            output_columns.push(retraction_array as ArrayRef);

            let new_batch = RecordBatch::try_new(
                output_schema,
                output_columns,
            ).unwrap();

            produce(&ProduceContext {}, new_batch);

            Ok(())
        }, &mut noop_meta_send);

        Ok(())
    }
}

pub trait Trigger: std::fmt::Debug {
    fn keys_received(&mut self, keys: Vec<ArrayRef>);
    fn poll(&mut self) -> Vec<ArrayRef>;
}

#[derive(Debug)]
pub struct CountingTrigger {
    key_data_types: Vec<DataType>,
    trigger_count: i64,
    counts: BTreeMap<Vec<GroupByScalar>, i64>,
    to_trigger: BTreeSet<Vec<GroupByScalar>>,
}

impl CountingTrigger {
    fn new(key_data_types: Vec<DataType>,
           trigger_count: i64) -> CountingTrigger {
        CountingTrigger {
            key_data_types,
            trigger_count,
            counts: Default::default(),
            to_trigger: Default::default(),
        }
    }
}

impl Trigger for CountingTrigger {
    fn keys_received(&mut self, keys: Vec<ArrayRef>) {
        let mut key_vec: Vec<GroupByScalar> = Vec::with_capacity(keys.len());
        for i in 0..self.key_data_types.len() {
            key_vec.push(GroupByScalar::Int64(0))
        }

        for row in 0..keys[0].len() {
            create_key(keys.as_slice(), row, &mut key_vec);

            let count = self.counts
                .entry(key_vec.clone())
                .or_insert(0);
            *count += 1;
            if *count == self.trigger_count {
                *count = 0; // TODO: Delete
                self.to_trigger.insert(key_vec.clone());
            }
        }
    }

    fn poll(&mut self) -> Vec<ArrayRef> {
        let mut output_columns: Vec<ArrayRef> = Vec::with_capacity(self.key_data_types.len());
        for key_index in 0..self.key_data_types.len() {
            match self.key_data_types[key_index] {
                DataType::Utf8 => {
                    let mut array = StringBuilder::new(self.to_trigger.len());
                    self.to_trigger
                        .iter()
                        .for_each(|k| {
                            match &k[key_index] {
                                GroupByScalar::Utf8(text) => array.append_value(text.as_str()).unwrap(),
                                _ => panic!("bug: key doesn't match schema"),
                                // TODO: Maybe use as_any -> downcast?
                            }
                        });
                    output_columns.push(Arc::new(array.finish()) as ArrayRef);
                }
                DataType::Int64 => {
                    let mut array = Int64Builder::new(self.to_trigger.len());
                    self.to_trigger
                        .iter()
                        .for_each(|k| {
                            match k[key_index] {
                                GroupByScalar::Int64(n) => array.append_value(n).unwrap(),
                                _ => panic!("bug: key doesn't match schema"),
                                // TODO: Maybe use as_any -> downcast?
                            }
                        });
                    output_columns.push(Arc::new(array.finish()) as ArrayRef);
                }
                _ => unimplemented!(),
            }
        }
        self.to_trigger.clear();
        output_columns
    }
}

fn main() {
    let start_time = std::time::Instant::now();

    let plan: Box<dyn Node> = Box::new(CSVSource::new("cats.csv"));
    //let plan: Box<dyn Node> = Box::new(Projection::new(&["id", "name"], plan));
    // let plan: Box<dyn Node> = Box::new(GroupBy::new(
    //     vec![String::from("name"), String::from("description"), String::from("age")],
    //     vec![String::from("livesleft")],
    //     vec![Box::new(Sum {})],
    //     vec![String::from("livesleft")],
    //     plan,
    // ));
    let plan: Box<dyn Node> = Box::new(GroupBy::new(
        vec![String::from("name")],
        vec![String::from("livesleft")],
        vec![Box::new(Sum {})],
        vec![String::from("livesleft")],
        plan,
    ));
    let res = plan.run(&ExecutionContext {}, &mut record_print, &mut noop_meta_send);
    println!("{:?}", start_time.elapsed());
}


// ****** Copied from datafusion

/// Enumeration of types that can be used in a GROUP BY expression (all primitives except
/// for floating point numerics)
#[derive(Debug, PartialEq, Eq, Hash, Clone, Ord, PartialOrd)]
enum GroupByScalar {
    UInt8(u8),
    UInt16(u16),
    UInt32(u32),
    UInt64(u64),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Utf8(String),
}

/// Create a Vec<GroupByScalar> that can be used as a map key
fn create_key(
    group_by_keys: &[ArrayRef],
    row: usize,
    vec: &mut Vec<GroupByScalar>,
) -> Result<(), Error> {
    for i in 0..group_by_keys.len() {
        let col = &group_by_keys[i];
        match col.data_type() {
            DataType::UInt8 => {
                let array = col.as_any().downcast_ref::<UInt8Array>().unwrap();
                vec[i] = GroupByScalar::UInt8(array.value(row))
            }
            DataType::UInt16 => {
                let array = col.as_any().downcast_ref::<UInt16Array>().unwrap();
                vec[i] = GroupByScalar::UInt16(array.value(row))
            }
            DataType::UInt32 => {
                let array = col.as_any().downcast_ref::<UInt32Array>().unwrap();
                vec[i] = GroupByScalar::UInt32(array.value(row))
            }
            DataType::UInt64 => {
                let array = col.as_any().downcast_ref::<UInt64Array>().unwrap();
                vec[i] = GroupByScalar::UInt64(array.value(row))
            }
            DataType::Int8 => {
                let array = col.as_any().downcast_ref::<Int8Array>().unwrap();
                vec[i] = GroupByScalar::Int8(array.value(row))
            }
            DataType::Int16 => {
                let array = col.as_any().downcast_ref::<Int16Array>().unwrap();
                vec[i] = GroupByScalar::Int16(array.value(row))
            }
            DataType::Int32 => {
                let array = col.as_any().downcast_ref::<Int32Array>().unwrap();
                vec[i] = GroupByScalar::Int32(array.value(row))
            }
            DataType::Int64 => {
                let array = col.as_any().downcast_ref::<Int64Array>().unwrap();
                vec[i] = GroupByScalar::Int64(array.value(row))
            }
            DataType::Utf8 => {
                let array = col.as_any().downcast_ref::<StringArray>().unwrap();
                vec[i] = GroupByScalar::Utf8(String::from(array.value(row)))
            }
            _ => {
                return Err(Error::Unexpected);
            }
        }
    }
    Ok(())
}
