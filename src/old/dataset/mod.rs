use std::collections::HashMap;
use std::hash::Hash;
use std::io::{Read, BufReader};
use std::fs::File;
use std::time::Instant;

use flate2::read::GzDecoder;

use conf::{Config, Objective};
use bits::{BitSet, BitVec, BitSlice};

use log::info;

pub type NomT = u32; // nominal type
pub type NumT = f32; // numerical type

pub enum FeatureData {
    /// Low cardinal nominal data: bit sets for one-hot encoding. Store a bitset for each value.
    BitSets(Vec<(NomT, BitSet)>),

    /// Low precision ordinal feature (up to 4 bits per feature value).
    BitSlice(BitSlice),

    /// Plain numerical feature (e.g. regression target feature).
    Numerical(Vec<NumT>),
}

pub struct Feature {
    id: usize,
    name: String,
    data: FeatureData,
}

pub struct DataSetBuilder<'a> {
    config: &'a Config,
    len: usize,
    input_features: Vec<Feature>,
    target_feature: Option<Feature>,
}

pub struct DataSet {
    len: usize,
    input_features: Vec<Feature>,
    target_feature: Feature,
}




// - Feature impl ---------------------------------------------------------------------------------

impl Feature {
    pub fn get_name(&self) -> &str { &self.name }
    pub fn get_id(&self) -> usize { self.id }
    pub fn get_data(&self) -> &FeatureData { &self.data }

    pub fn set_feature_name(&mut self, name: &str) { self.name = String::from(name); }
    pub fn set_data(&mut self, data: FeatureData) { self.data = data; }

    pub fn len(&self) -> usize {
        match self.data {
            FeatureData::BitSets(ref tuples) => tuples.iter().next().map(|t| t.1.nbits()).unwrap(),
            FeatureData::BitSlice(ref bitslice) => bitslice.nbits(),
            FeatureData::Numerical(ref vec) => vec.len(),
        }
    }

    pub fn get_value(&self, index: usize) -> NumT {
        match self.data {
            FeatureData::BitSets(ref tuples) => {
                tuples.iter()
                    .filter(|(v, bs)| bs.get_bit(index))
                    .map(|(v, _)| *v as NumT)
                    .next().unwrap()
            },
            FeatureData::BitSlice(ref slice) => {
                slice.get_value(index) as NumT
            },
            FeatureData::Numerical(ref vec) => {
                vec[index]
            }
        }
    }
}






// - DataSetBuilder impl --------------------------------------------------------------------------

impl <'a> DataSetBuilder<'a> {
    pub fn new(config: &'a Config) -> DataSetBuilder {
        DataSetBuilder {
            config: config,
            len: 0,
            input_features: Vec::new(),
            target_feature: None,
        }
    }

    pub fn from_gzip_csv_file(config: &'a Config, filename: &str) -> Result<DataSet, String> {
        info!("Reading gzipped CSV data file {}", filename);
        let reader = BufReader::new(try_or_str!(File::open(filename), "cannot open CSV.GZ file"));
        Self::from_gzip_csv(config, reader)
    }

    pub fn from_csv_file(config: &'a Config, filename: &str) -> Result<DataSet, String> {
        info!("Reading CSV data file {}", filename);
        let reader = BufReader::new(try_or_str!(File::open(filename), "cannot open CSV file"));
        Self::from_csv(config, reader)
    }

    pub fn from_gzip_csv<R>(config: &'a Config, gz_reader: R) -> Result<DataSet, String>
    where R: Read {
        Self::from_csv(config, GzDecoder::new(gz_reader))
    }

    pub fn from_csv<R>(config: &'a Config, csv_reader: R) -> Result<DataSet, String>
    where R: Read {
        let mut rdr = csv::Reader::from_reader(csv_reader);

        // Read CSV file and cache in vecs
        let start = Instant::now();
        let columns = Self::buffer_records_as_columns(&mut rdr)?;
        let ncolumns = columns.len();
        let elapsed = start.elapsed();
        let target_i = if config.target_feature >= 0 { config.target_feature as usize }
                       else { ncolumns - ((-config.target_feature) as usize) };

        info!("Target feature id is {}", target_i);

        println!("dataset buffered in {}", (elapsed.as_secs() as f64 * 1e6 +
                                            elapsed.subsec_micros() as f64) / 1e6);

        let start = Instant::now();

        // Construct feature columns
        let mut builder = DataSetBuilder::new(config);
        for (i, mut column) in columns.into_iter().enumerate() {
            if target_i == i {
                match builder.config.objective {
                    Objective::Regression => {
                        builder.add_regression_target(column.into_iter())?;
                    },
                    Objective::Classification => {
                        let len = column.len();
                        let target = column.into_iter().map(|e| e == 1.0);
                        builder.add_classification_target(len, target)?;
                    }
                }
            } else if builder.config.ignored_features.contains(&i) {
            } else if builder.config.lowcard_nominal_features.contains(&i) {
                let len = column.len();
                let feature = column.into_iter().map(|e| e.round() as NomT);
                builder.add_lowcard_nominal_feature(len, feature)?;
            } else {
                unimplemented!();
            }
        }

        let elapsed = start.elapsed();
        println!("CSV dataset parsed in {}s", (elapsed.as_secs() as f64 * 1e6 +
                                               elapsed.subsec_micros() as f64) / 1e6);
        builder.into_dataset()
    }

    fn buffer_records_as_columns<R>(rdr: &mut csv::Reader<R>) -> Result<Vec<Vec<NumT>>, String>
    where R: Read {
        let mut columns: Vec<Vec<NumT>> = Vec::new();
        for result in rdr.records() {
            let record = try_or_str!(result, "error parsing CSV record");
            if columns.len() == 0 {
                columns = vec![Vec::new(); record.len()];
            }

            for (i, v) in record.iter().enumerate() {
                columns[i].push(try_or_str!(v.parse::<NumT>(), "float parse error"));
            }
        }
        Ok(columns)
    }

    pub fn into_dataset(self) -> Result<DataSet, String> {
        // No input features
        if self.input_features.is_empty() { return Err(String::from("no input features")); }

        // There must be a target feature set
        let target = self.target_feature.ok_or("no target feature set")?;

        // Length must be greater than zero
        let len = self.len;
        if len == 0 { return Err(String::from("features of len 0")); }

        info!("Dataset with {} features and {} examples", self.input_features.len(), len);

        Ok(DataSet {
            len: len,
            input_features: self.input_features,
            target_feature: target,
        })
    }

    fn check_and_update_length(&mut self, len: usize) -> Result<(), String> {
        if len == 0 {
            Err(String::from("feature with length zero"))
        } else {
            self.len = if self.len == 0 { len }  else { usize::min(self.len, len) };
            Ok(())
        }
    }

    fn new_lowcard_nominal_feature<I>(&mut self, len: usize, iter: I) -> Result<Feature, String>
    where I: Iterator,
          I::Item: Copy + Eq + Hash + Into<NomT>
    {
        self.check_and_update_length(len)?;

        let mut map: HashMap<I::Item, BitVec> = HashMap::new();
        
        // Loop over feature data in `iter` and construct bitsets for each possible value
        for (i, v) in iter.enumerate() {
            if !map.contains_key(&v) {
                if map.len() + 1 > self.config.max_lowcard_nominal_cardinality {
                    return Err(format!("low cardinality nominal feature with more than {} distinct
                                       values", self.config.max_lowcard_nominal_cardinality));
                }
                map.insert(v, BitVec::zero_bits(len));
            }

            if let Some(bs) = map.get_mut(&v) {
                bs.set_bit(i, true);
            } else {
                return Err(String::from("lowcard feature construction hash error"));
            }
        }

        if map.is_empty() { return Err(String::from("lowcard feature empty")); }
        
        // Construct the feature
        let mut bitset_vec: Vec<(NomT, BitSet)> = map.into_iter()
            .map(|p| (p.0.into(), p.1.into_bitset(len)))
            .collect();
        bitset_vec.sort_by(|p, q| p.0.cmp(&q.0));

        let feature = Feature {
            id: 0,
            name: String::new(),
            data: FeatureData::BitSets(bitset_vec),
        };
        
        Ok(feature)
    }

    fn new_numerical_feature<I>(&mut self, iter: I) -> Result<Feature, String>
    where I: Iterator<Item = NumT> {
        let values = iter.collect::<Vec<NumT>>();
        self.check_and_update_length(values.len())?;
        let feature = Feature {
            id: 0,
            name: String::new(),
            data: FeatureData::Numerical(values),
        };
        Ok(feature)
    }

    /// Add a new low cardinality nominal feature. The feature id is returned.
    pub fn add_lowcard_nominal_feature<I>(&mut self, len: usize, iter: I) -> Result<usize, String>
    where I: Iterator,
          I::Item: Copy + Eq + Hash + Into<NomT>
    {
        let mut feature = self.new_lowcard_nominal_feature(len, iter)?;
        let id = self.input_features.len();
        feature.id = id;
        self.input_features.push(feature);
        info!("Added low cardinality input feature with id={}", id);
        Ok(id)
    }

    pub fn add_regression_target<I>(&mut self, iter: I) -> Result<(), String>
    where I: Iterator<Item = NumT> {
        let feature = self.new_numerical_feature(iter)?;
        self.target_feature = Some(feature);
        info!("Added regression target feature");
        Ok(())
    }

    pub fn add_classification_target<I>(&mut self, len: usize, iter: I) -> Result<(), String>
    where I: Iterator<Item = bool> {
        let feature = self.new_lowcard_nominal_feature(len, iter)?;
        self.target_feature = Some(feature);
        info!("Added classification target feature");
        Ok(())
    }
}






// - DataSet impl ---------------------------------------------------------------------------------

impl DataSet {
    pub fn nexamples(&self) -> usize { self.len }
    pub fn get_feature(&self, id: usize) -> &Feature { &self.input_features[id] }
    pub fn get_feature_mut(&mut self, id: usize) -> &mut Feature { &mut self.input_features[id] }

    pub fn ninput_features(&self) -> usize { self.input_features.len() }
    pub fn get_input_features(&self) -> impl Iterator<Item = &Feature> {
        self.input_features.iter()
    }

    pub fn get_target_feature(&self) -> &Feature {
        &self.target_feature
    }

    pub fn get_value(&self, feat_id: usize, index: usize) -> NumT {
        self.get_feature(feat_id).get_value(index)
    }
}











// - Tests ----------------------------------------------------------------------------------------
#[cfg(test)]
mod test {
    use dataset::{DataSetBuilder, FeatureData};
    use conf::Config;
    use std::default::Default;

    #[test]
    fn test_add_lowcard_nominal_feature() {
        let mut conf = Config::default();
        conf.max_lowcard_nominal_cardinality = 3;
        let mut builder = DataSetBuilder::new(&conf);
        let raw_data = vec![1i64, 2, 1, 1, 2, 2, 2, 3];
        let raw_target = vec![0f32; 8];
        let id = builder.add_lowcard_nominal_feature(
            raw_data.len(), raw_data.into_iter()).unwrap();
        builder.add_regression_target(raw_target.into_iter()).unwrap();

        let dataset = builder.into_dataset().unwrap();
        let feature = dataset.get_feature(id);

        if let FeatureData::BitSets(bitsets) = feature.get_data() {
            assert_eq!(1i64, bitsets[0].0);
            assert_eq!(2i64, bitsets[1].0);
            assert_eq!(3i64, bitsets[2].0);
            assert_eq!(0b00001101, bitsets[0].1.cast::<u8>()[0]);
            assert_eq!(0b01110010, bitsets[1].1.cast::<u8>()[0]);
            assert_eq!(0b10000000, bitsets[2].1.cast::<u8>()[0]);
        } else { panic!(); }
    }

    #[test] #[should_panic]
    fn test_add_lowcard_nominal_feature_too_many() {
        let mut conf = Config::default();
        conf.max_lowcard_nominal_cardinality = 2;
        let mut dataset = DataSetBuilder::new(&conf);
        let raw_data = vec![1i64, 2, 1, 1, 2, 2, 2, 3];
        dataset.add_lowcard_nominal_feature(raw_data.len(), raw_data.into_iter()).unwrap();
    }
}