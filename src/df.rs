#[cfg(feature = "arrow2_ih")]
extern crate arrow2_ih as arrow2;

use crate::{Error, Time, TimeZone};
use arrow2::array::{Array, Float64Array, Int64Array, Utf8Array};
pub use arrow2::chunk::Chunk;
use arrow2::datatypes::Field;
pub use arrow2::datatypes::{DataType, Metadata, Schema, TimeUnit};
use arrow2::error::Error as ArrowError;
use arrow2::io::ipc::read::{StreamReader, StreamState};
use arrow2::io::ipc::write::{StreamWriter, WriteOptions};
use chrono::{DateTime, Local, NaiveDateTime, SecondsFormat, Utc};

/// Series type, alias for boxed arrow2 array
///
/// The series can contain a single array only. If more arrays required in a column, consider
/// creating a new dataframe
pub type Series = Box<(dyn Array + 'static)>;

/// Base data frame class
///
/// The data frame can be automatically converted into:
///
/// IPC chunk (Chunk::from)
/// Ready-to-send IPC block (Vec<u8>::from)
/// Polars data frame (polars::frame::DateFrame::from, "polars" feature required)
#[derive(Default, Clone)]
pub struct DataFrame {
    fields: Vec<Field>,
    data: Vec<Series>,
    rows: usize,
}

macro_rules! convert {
    ($df: expr, $index: expr, $arr: tt, $dt: expr) => {
        if let Some(series) = $df.data.get($index) {
            let values: &Utf8Array<i64> = series
                .as_any()
                .downcast_ref()
                .ok_or_else(|| Error::TypeMismatch)?;
            let mut dt: Vec<Option<_>> = Vec::with_capacity(values.len());
            for val in values {
                dt.push(if let Some(s) = val {
                    s.parse().ok()
                } else {
                    None
                });
            }
            $df.data[$index] = $arr::from(dt).boxed();
            $df.fields[$index].data_type = $dt;
            Ok(())
        } else {
            Err(Error::OutOfBounds)
        }
    };
}

impl DataFrame {
    /// Create a new data frame with fixed number of rows and no columns
    #[inline]
    pub fn new0(rows: usize) -> Self {
        Self::new(rows, None)
    }
    /// Create a new data frame with fixed number of rows and allocate columns
    #[inline]
    pub fn new(rows: usize, cols: Option<usize>) -> Self {
        Self {
            data: Vec::with_capacity(cols.unwrap_or_default()),
            rows,
            fields: Vec::with_capacity(cols.unwrap_or_default()),
        }
    }
    /// Create a new time-series data frame from f64 timestamps
    ///
    /// # Panics
    ///
    /// should not panic
    pub fn new_timeseries_from_float(
        time_series: Vec<f64>,
        cols: Option<usize>,
        tz: TimeZone,
        time_unit: TimeUnit,
    ) -> Self {
        let mut df = Self::new(time_series.len(), cols.map(|c| c + 1));
        #[allow(clippy::cast_possible_truncation)]
        #[allow(clippy::cast_possible_wrap)]
        let ts = Int64Array::from(
            time_series
                .into_iter()
                .map(|v| {
                    Some({
                        match time_unit {
                            TimeUnit::Second => v.trunc() as i64,
                            TimeUnit::Millisecond => {
                                let t = Time::from_timestamp(v);
                                t.timestamp_ms() as i64
                            }
                            TimeUnit::Microsecond => {
                                let t = Time::from_timestamp(v);
                                t.timestamp_us() as i64
                            }
                            TimeUnit::Nanosecond => {
                                let t = Time::from_timestamp(v);
                                t.timestamp_ns() as i64
                            }
                        }
                    })
                })
                .collect::<Vec<Option<i64>>>(),
        )
        .boxed();
        df.add_series("time", ts, DataType::Timestamp(time_unit, tz.into()))
            .unwrap();
        df
    }
    /// Create a new time-series data frame from f64 timestamps and convert them to rfc3339 strings
    ///
    /// # Panics
    ///
    /// should not panic
    pub fn new_timeseries_from_float_rfc3339(time_series: Vec<f64>, cols: Option<usize>) -> Self {
        let mut df = Self::new(time_series.len(), cols.map(|c| c + 1));
        let ts: Vec<Option<String>> = time_series
            .iter()
            .map(|v| {
                #[allow(clippy::cast_possible_truncation)]
                #[allow(clippy::cast_sign_loss)]
                let dt_utc = DateTime::<Utc>::from_utc(
                    NaiveDateTime::from_timestamp_opt(
                        v.trunc() as i64,
                        (v.fract() * 1_000_000_000.0) as u32,
                    )
                    .unwrap_or_default(),
                    Utc,
                );
                let dt: DateTime<Local> = DateTime::from(dt_utc);
                Some(dt.to_rfc3339_opts(SecondsFormat::Secs, true))
            })
            .collect();
        df.add_series0("time", Utf8Array::<i32>::from(ts).boxed())
            .unwrap();
        df
    }
    /// Create a data frame from IPC chunk and schema
    pub fn from_chunk(chunk: Chunk<Box<dyn Array + 'static>>, schema: &Schema) -> Self {
        let data = chunk.into_arrays();
        let rows = data.first().map_or(0, |v| v.len());
        Self {
            fields: schema.fields.clone(),
            data,
            rows,
        }
    }
    /// Create a data frame from vector of fields and vector of series
    pub fn from_parts(fields: Vec<Field>, data: Vec<Series>) -> Result<Self, Error> {
        let rows = if let Some(x) = data.first() {
            let rows = x.len();
            for s in data.iter().skip(1) {
                if s.len() != rows {
                    return Err(Error::RowsNotMatch);
                }
            }
            rows
        } else {
            0
        };
        Ok(Self { fields, data, rows })
    }
    /// Split the data frame into vector of fields and vector of series
    pub fn into_parts(self) -> (Vec<Field>, Vec<Series>) {
        (self.fields, self.data)
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    /// Column names
    #[inline]
    pub fn names(&self) -> Vec<&str> {
        self.fields.iter().map(|col| col.name.as_str()).collect()
    }
    /// Column field objects
    #[inline]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }
    /// Columns (data)
    #[inline]
    pub fn data(&self) -> &[Series] {
        &self.data
    }
    /// Add series to the data frame as a new column and specify its type
    pub fn add_series(
        &mut self,
        name: &str,
        series: Series,
        data_type: DataType,
    ) -> Result<(), Error> {
        if series.len() == self.rows {
            self.fields.push(Field::new(name, data_type, true));
            self.data.push(series);
            Ok(())
        } else {
            Err(Error::RowsNotMatch)
        }
    }
    /// Add series to the data frame as a new column and use the same type as the series
    #[inline]
    pub fn add_series0(&mut self, name: &str, series: Series) -> Result<(), Error> {
        let dt = series.data_type().clone();
        self.add_series(name, series, dt)
    }
    /// Insert series to the data frame as a new column and specify its type
    pub fn insert_series(
        &mut self,
        name: &str,
        series: Series,
        index: usize,
        data_type: DataType,
    ) -> Result<(), Error> {
        if index <= self.data.len() {
            if series.len() == self.rows {
                self.fields.insert(index, Field::new(name, data_type, true));
                self.data.insert(index, series);
                Ok(())
            } else {
                Err(Error::RowsNotMatch)
            }
        } else {
            Err(Error::OutOfBounds)
        }
    }
    /// Add series to the data frame as a new column and use the same type as the series
    #[inline]
    pub fn insert_series0(
        &mut self,
        name: &str,
        series: Series,
        index: usize,
    ) -> Result<(), Error> {
        let dt = series.data_type().clone();
        self.insert_series(name, series, index, dt)
    }
    /// Create a vector of sliced series
    pub fn try_series_sliced(&self, offset: usize, length: usize) -> Result<Vec<Series>, Error> {
        if offset + length <= self.rows {
            Ok(self.data.iter().map(|d| d.sliced(offset, length)).collect())
        } else {
            Err(Error::OutOfBounds)
        }
    }
    /// Create IPC chunk of sliced series
    #[inline]
    pub fn try_chunk_sliced(
        &self,
        offset: usize,
        length: usize,
    ) -> Result<Chunk<Box<dyn Array>>, Error> {
        let series = self.try_series_sliced(offset, length)?;
        Ok(Chunk::new(series))
    }
    /// Create a new data frame of sliced series
    pub fn try_sliced(&self, offset: usize, length: usize) -> Result<Self, Error> {
        if offset + length <= self.rows {
            Ok(Self {
                data: self.data.iter().map(|d| d.sliced(offset, length)).collect(),
                rows: length,
                fields: self.fields.clone(),
            })
        } else {
            Err(Error::OutOfBounds)
        }
    }
    /// Generate schema object
    #[inline]
    pub fn schema(&self) -> Schema {
        Schema::from(self.fields.clone())
    }
    #[inline]
    pub fn rows(&self) -> usize {
        self.rows
    }
    /// calculate approx data frame size
    ///
    /// (does not work properly for strings)
    pub fn size(&self) -> usize {
        let mut size = 0;
        for d in &self.data {
            let m = match d.data_type() {
                DataType::Boolean => 1,
                DataType::Int16 => 2,
                DataType::Int32 | DataType::Float32 => 4,
                _ => 8,
            };
            size += d.len() * m;
        }
        size
    }
    /// Get column index
    #[inline]
    pub fn get_column_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|v| v.name == name)
    }
    /// Set column ordering
    pub fn set_ordering(&mut self, names: &[&str]) {
        for (i, name) in names.iter().enumerate() {
            if let Some(pos) = self.get_column_index(name) {
                if pos != i {
                    self.fields.swap(i, pos);
                    self.data.swap(i, pos);
                }
            }
        }
    }
    /// Sort columns alphabetically
    pub fn sort_columns(&mut self) {
        let mut names = self
            .fields
            .iter()
            .map(|v| v.name.clone())
            .collect::<Vec<String>>();
        names.sort();
        self.set_ordering(&names.iter().map(String::as_str).collect::<Vec<&str>>());
    }
    /// Convert into IPC parts: schema + chunk
    pub fn into_ipc_parts(self) -> (Schema, Chunk<Box<dyn Array + 'static>>) {
        let schema = Schema::from(self.fields);
        let chunk = Chunk::new(self.data);
        (schema, chunk)
    }
    /// Convert into IPC ready-to-send block
    pub fn into_ipc_block(self) -> Result<Vec<u8>, ArrowError> {
        let mut buf = Vec::new();
        let schema = self.schema();
        let chunk = Chunk::new(self.data);
        let mut writer = StreamWriter::new(&mut buf, WriteOptions::default());
        writer.start(&schema, None)?;
        writer.write(&chunk, None)?;
        writer.finish()?;
        Ok(buf)
    }
    /// Create a data frame from a complete IPC block
    pub fn from_ipc_block(block: &[u8]) -> Result<(Self, Metadata), ArrowError> {
        let mut buf = std::io::Cursor::new(block);
        let meta = arrow2::io::ipc::read::read_stream_metadata(&mut buf)?;
        let reader = StreamReader::new(buf, meta, None);
        let fields = reader.metadata().schema.fields.clone();
        let metadata = reader.metadata().schema.metadata.clone();
        for state in reader {
            match state? {
                StreamState::Waiting => continue,
                StreamState::Some(chunk) => {
                    let data = chunk.into_arrays();
                    let rows = data.first().map_or(0, |v| v.len());
                    return Ok((Self { fields, data, rows }, metadata));
                }
            }
        }
        Ok((DataFrame::new0(0), metadata))
    }
    /// Pop series by name
    pub fn pop_series(&mut self, name: &str) -> Result<(Series, DataType), Error> {
        if let Some((pos, _)) = self
            .fields
            .iter()
            .enumerate()
            .find(|(_, field)| field.name == name)
        {
            let field = self.fields.remove(pos);
            Ok((self.data.remove(pos), field.data_type))
        } else {
            Err(Error::NotFound(name.to_owned()))
        }
    }
    /// Pop series by index
    pub fn pop_series_at(&mut self, index: usize) -> Result<(Series, String, DataType), Error> {
        if index < self.fields.len() {
            let field = self.fields.remove(index);
            Ok((self.data.remove(index), field.name, field.data_type))
        } else {
            Err(Error::OutOfBounds)
        }
    }
    /// Rename column
    pub fn rename(&mut self, name: &str, new_name: &str) -> Result<(), Error> {
        if let Some(field) = self.fields.iter_mut().find(|field| field.name == name) {
            field.name = new_name.to_owned();
            Ok(())
        } else {
            Err(Error::NotFound(name.to_owned()))
        }
    }
    /// Parse string column values to integers
    pub fn parse_int(&mut self, name: &str) -> Result<(), Error> {
        if let Some(pos) = self.get_column_index(name) {
            self.parse_int_at(pos)
        } else {
            Err(Error::NotFound(name.to_owned()))
        }
    }
    /// Parse string column values to floats
    pub fn parse_float(&mut self, name: &str) -> Result<(), Error> {
        if let Some(pos) = self.get_column_index(name) {
            self.parse_float_at(pos)
        } else {
            Err(Error::NotFound(name.to_owned()))
        }
    }
    /// Parse string column values to integers
    pub fn parse_int_at(&mut self, index: usize) -> Result<(), Error> {
        convert!(self, index, Int64Array, DataType::Int64)
    }
    /// Parse string column values to floats
    pub fn parse_float_at(&mut self, index: usize) -> Result<(), Error> {
        convert!(self, index, Float64Array, DataType::Float64)
    }
    /// Set field name by index
    pub fn set_name_at(&mut self, index: usize, new_name: &str) -> Result<(), Error> {
        if let Some(field) = self.fields.get_mut(index) {
            field.name = new_name.to_owned();
            Ok(())
        } else {
            Err(Error::OutOfBounds)
        }
    }
    /// Override field data type
    pub fn set_data_type(&mut self, name: &str, data_type: DataType) -> Result<(), Error> {
        if let Some(field) = self.fields.iter_mut().find(|field| field.name == name) {
            field.data_type = data_type;
            Ok(())
        } else {
            Err(Error::NotFound(name.to_owned()))
        }
    }
    /// Override field data type by index
    pub fn set_data_type_at(&mut self, index: usize, data_type: DataType) -> Result<(), Error> {
        if let Some(field) = self.fields.get_mut(index) {
            field.data_type = data_type;
            Ok(())
        } else {
            Err(Error::OutOfBounds)
        }
    }
}

impl From<DataFrame> for Chunk<Box<dyn Array>> {
    #[inline]
    fn from(df: DataFrame) -> Self {
        Chunk::new(df.data)
    }
}

impl TryFrom<DataFrame> for Vec<u8> {
    type Error = ArrowError;
    #[inline]
    fn try_from(df: DataFrame) -> Result<Self, Self::Error> {
        df.into_ipc_block()
    }
}

#[cfg(feature = "polars")]
impl From<DataFrame> for polars::frame::DataFrame {
    fn from(df: DataFrame) -> polars::frame::DataFrame {
        let (fields, data) = df.into_parts();
        let polars_series = unsafe {
            data.into_iter()
                .zip(fields)
                .map(|(d, f)| {
                    polars::series::Series::from_chunks_and_dtype_unchecked(
                        &f.name,
                        vec![d],
                        &f.data_type().into(),
                    )
                })
                .collect::<Vec<polars::series::Series>>()
        };
        polars::frame::DataFrame::new_no_checks(polars_series)
    }
}

#[cfg(feature = "polars")]
impl From<polars::frame::DataFrame> for DataFrame {
    fn from(mut polars_df: polars::frame::DataFrame) -> DataFrame {
        match polars_df.n_chunks() {
            0 => return DataFrame::new0(0),
            2.. => polars_df = polars_df.agg_chunks(),
            _ => {}
        }
        let pl_series: Vec<polars::series::Series> = polars_df.into();
        let names: Vec<String> = pl_series.iter().map(|s| s.name().to_owned()).collect();
        let series: Vec<Series> = pl_series.into_iter().map(|v| v.to_arrow(0)).collect();
        let mut df = DataFrame::new(series.first().map_or(0, |s| s.len()), Some(series.len()));
        for (s, name) in series.into_iter().zip(names) {
            df.add_series0(&name, s).unwrap();
        }
        df
    }
}
