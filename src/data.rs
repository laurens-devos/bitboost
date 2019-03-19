use std::io::Read;
use std::path::Path;
use std::fs::File;
use std::rc::Rc;
use std::cmp::Ordering;

use csv;

use crate::{NumT, CatT, POS_INF, NEG_INF, into_cat, EPSILON};
use crate::config::Config;
use crate::slice_store::{SliceRange, BitBlockStore, BitVecRef};
use crate::binner::Binner;
use crate::simd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatType {
    LoCardCat,
    HiCardCat,
    Numerical,
}

/// The last feature is considered the target feature.
pub struct Data {
    names: Vec<String>,
    nfeatures: usize,
    nexamples: usize,
    features: Vec<Vec<NumT>>,
    limits: Vec<(NumT, NumT)>, // feature min / max value
    ftypes: Vec<FeatType>,
    cards: Vec<usize>, // only for categorical
}

impl Data {
    pub fn from_csv_path<P: AsRef<Path>>(config: &Config, path: P) -> Result<Data, String> {
        let reader = File::open(path).map_err(|err| format!("path error: {}", err))?;
        Data::from_csv_reader(config, reader)
    }

    pub fn from_csv_reader<R>(config: &Config, mut reader: R) -> Result<Data, String>
    where R: Read
    {
        let mut csv = String::new();
        reader.read_to_string(&mut csv).map_err(|err| format!("read_to_string err: {}", err))?;
        Self::from_csv(config, &csv)
    }

    pub fn from_csv(config: &Config, csv: &str) -> Result<Data, String> {
        let mut record_len = 0;
        let mut record_count = 0;
        let mut features = Vec::<Vec<NumT>>::new();
        let mut limits = Vec::new();
        let mut ftypes = Vec::new();
        let mut cards = Vec::new();
        let mut record = csv::StringRecord::new();
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(config.csv_has_header)
            .delimiter(config.csv_delimiter)
            .from_reader(csv.as_bytes());

        loop {
            match reader.read_record(&mut record) {
                Ok(false) => break,
                Err(err) => return Err(format!("CSV error: {}", err)),
                Ok(true) => {
                    if record_count == 0 {
                        record_len = record.len();
                        features.resize(record_len, Vec::new());
                        limits.resize(record_len, (POS_INF, NEG_INF));
                        ftypes.resize(record_len, FeatType::Numerical);
                        config.categorical_features.iter()
                            .for_each(|&c| if c<record_len { ftypes[c] = FeatType::LoCardCat; });
                        cards.resize(record_len, 0);
                    }

                    for i in 0..record_len {
                        let value = record.get(i)
                            .and_then(|x| x.parse::<NumT>().ok())
                            .ok_or(format!("number error at record {}", record_count))?;

                        features[i].push(value);
                        limits[i] = { let l = limits[i]; (l.0.min(value), l.1.max(value)) };
                        if ftypes[i] == FeatType::LoCardCat {
                            if value.round() != value || value < 0.0 {
                                return Err(format!("Invalid categorical value {} at record {}",
                                           value, record_count));
                            }
                            cards[i] = cards[i].max(1 + into_cat(value) as usize);
                        }
                    }
                }
            }
            record_count += 1;
        }

        // update feature types to high cardinality categorical if cards exceeds max_nbins
        for j in 0..record_len {
            if cards[j] > config.max_nbins {
                debug_assert!(ftypes[j] == FeatType::LoCardCat);
                ftypes[j] = FeatType::HiCardCat;
            }
        }

        // extract feature names from header
        let mut names = vec![String::new(); record_len];
        if config.csv_has_header {
            reader.headers()
                .map_err(|err| format!("CSV header error: {}", err))?
                .into_iter()
                .enumerate()
                .for_each(|(i, name)| names[i].push_str(name));
        }

        Ok(Data {
            names,
            nfeatures: record_len - 1, // last is target
            nexamples: record_count,
            features,
            limits,
            ftypes,
            cards,
        })
    }

    pub fn nfeatures(&self) -> usize { self.nfeatures }
    pub fn nexamples(&self) -> usize { self.nexamples }
    pub fn feat_name(&self, feature: usize) -> &str { &self.names[feature] }
    pub fn feat_limits(&self, feat_id: usize) -> (NumT, NumT) { self.limits[feat_id] }
    pub fn feat_type(&self, feat_id: usize) -> FeatType { self.ftypes[feat_id] }
    pub fn feat_card(&self, feat_id: usize) -> usize { self.cards[feat_id] }
    pub fn target_id(&self) -> usize { self.nfeatures }
    pub fn get_feature(&self, feat_id: usize) -> &[NumT] { &self.features[feat_id] }
    pub fn get_target(&self) -> &[NumT] { &self.features[self.target_id()] }
}






// ------------------------------------------------------------------------------------------------



/// A 'Data' struct with all the necessary bitsets for training.
pub struct Dataset<'a> {
    max_nbins: usize,

    /// The original data as read from the data file.
    data: &'a Data,

    /// The gradients
    gradient: &'a [NumT],

    /// Min and max value of gradient
    gradient_lims: (NumT, NumT),

    /// Feature sub-selection.
    feat_sel: Vec<usize>,

    /// Bagging: which rows from 'data' are used in this dataset.
    example_sel: Vec<usize>,

    /// A store owning all bitsets.
    store: BitBlockStore,

    /// Bitsets for each feature, one for each possible value.
    bitvecs: Vec<Vec<SliceRange>>,

    /// For high cardinality categorical features, a list of hashsets is maintained containing all
    /// possible values per split (for an IN-SPLIT). For other features, this list is empty.
    super_categories: Vec<Rc<Vec<CatT>>>,

    /// For numerical features, store list of possible split values.
    split_values: Vec<Vec<NumT>>,

    /// Bins buffer for quantile approximation using Binner.
    bins_buffer_u32: Vec<u32>,
    bins_buffer_numt: Vec<NumT>,
}

impl <'a> Dataset<'a> {
    fn new(max_nbins: usize, data: &'a Data, gradient: &'a [NumT]) -> Dataset<'a> {
        Dataset {
            max_nbins,
            data,
            gradient,
            gradient_lims: (0.0, 0.0),
            feat_sel: Vec::new(),
            example_sel: Vec::new(),
            store: BitBlockStore::new(1024),
            bitvecs: Vec::new(),
            super_categories: Vec::new(),
            split_values: Vec::new(),
            bins_buffer_u32: vec![0; 1024],
            bins_buffer_numt: vec![0.0; 1024],
        }
    }

    fn reset(&mut self) {
        self.feat_sel.clear();
        self.example_sel.clear();
        self.store.reset();
        self.bitvecs.clear();
        self.super_categories.clear();
        self.split_values.clear();
    }

    pub fn construct_from_data(config: &Config, data: &'a Data, gradient: &'a [NumT])
        -> Dataset<'a>
    {
        let mut dataset = Dataset::new(config.max_nbins, data, gradient);
        dataset.construct_again_no_reset(config);
        dataset
    }

    pub fn construct_again(&mut self, config: &Config) {
        self.reset();
        self.construct_again_no_reset(config);
    }

    fn construct_again_no_reset(&mut self, config: &Config) {
        let n = self.data.nexamples();
        let m = self.data.nfeatures();
        let k = ((n as NumT) * config.example_fraction).round() as usize;
        let l = ((m as NumT) * config.feature_fraction).round() as usize;

        // Initializing data structures
        self.example_sel.resize(k, 0);
        self.feat_sel.resize(l, 0);
        self.bitvecs.resize(m, Vec::new());
        self.super_categories.resize(m, Rc::new(Vec::new()));
        self.split_values.resize(m, Vec::new());

        // Bagging and feature sub-selection
        if n == k { self.example_sel.iter_mut().enumerate().for_each(|(i, x)| *x = i); }
        else      { sample(n, &mut self.example_sel, config.random_seed); }
        reservoir_sample(m, &mut self.feat_sel, config.random_seed + 10);
        self.gradient_lims = self.example_sel.iter() // TODO objective also computes "bounds" of gradients
            .map(|&i| self.gradient[i])
            .fold((0.0, 0.0), |a, t| (a.0.min(t), a.1.max(t)));

        // Feature preprocessing
        for u in 0..l {
            let feat_id = self.feat_sel[u];

            // determine type of feature:
            // [1] low-cardinality categorical = explicitly categorical + card < max_nbins
            // [2] high-cardinality categorical = explicitly categorical + card >= max_nbins
            // [3] numerical = other
            match self.feat_type(feat_id) {
                FeatType::LoCardCat => self.preprocess_locard_cat(feat_id),
                FeatType::HiCardCat => self.preprocess_hicard_cat(feat_id),
                FeatType::Numerical => self.preprocess_num(feat_id),
            }
        }
    }

    /// Generate bitsets for each categorical value.
    fn preprocess_locard_cat(&mut self, feat_id: usize) {
        let n = self.nexamples();
        let data = self.data.get_feature(feat_id);
        let card = self.data.feat_card(feat_id);
        let iter = self.example_sel.iter().map(|&i| data[i]);
        let bitvecs = construct_bitvecs(&mut self.store, n, card, iter, |x| into_cat(x) as usize);
        self.bitvecs[feat_id] = bitvecs;
    }

    /// - Accumulate gradient mean for each categorical value.
    /// - Sort by accumulated value (-> becomes ordered)
    /// - Generate candidate split values using quantile estimates
    /// - Generate bitsets for IN-SPLITs
    fn preprocess_hicard_cat(&mut self, feat_id: usize) {
        let n = self.nexamples();
        let data = self.data.get_feature(feat_id);
        let card = self.data.feat_card(feat_id);
        let gradient = self.gradient;

        // collect gradient sums & counts per category value
        let mut grad_stat_pairs: Vec<(NumT, u32)> = vec![(0.0, 0); card];
        for (i, x) in self.example_sel.iter().map(|&i| data[i]).enumerate() {
            let category = into_cat(x) as usize;
            let entry = &mut grad_stat_pairs[category];
            entry.0 += gradient[i];
            entry.1 += 1;
        }

        // accumulate category values: mean -> this determines their ordering
        // combine similar categories using quantile estimations
        self.bins_buffer_u32.iter_mut().for_each(|b| *b = 0);
        let mut binner = Binner::new(&mut self.bins_buffer_u32, self.gradient_lims);
        let combiner = |bin: &mut u32, d: u32| *bin += d;
        for (sum, count) in grad_stat_pairs.iter_mut() {
            if *count != 0 {
                *sum /= *count as NumT;
                binner.insert(*sum, *count, combiner);
            }
        }

        // extract approximate quantiles from bins
        let extractor = |bin: &u32| *bin;
        let rank_step = n as NumT / (self.max_nbins + 1) as NumT;
        let ranks = (1..=self.max_nbins).map(|i| (i as NumT * rank_step).round() as u32 - 1);
        let qbins = binner.rank_iter(ranks, extractor);
        let mut last_bin = usize::max_value();
        let mut split_weights = Vec::with_capacity(self.max_nbins);
        for bin in qbins {
            if bin == last_bin { continue; }
            last_bin = bin;
            split_weights.push(binner.bin_representative(bin));
        }
        let super_card = split_weights.len();
        debug_assert!(super_card <= self.max_nbins);

        // generate mapping: category -> super category
        let mut super_categories = vec![0; card];
        for (category, &(mean, _))  in grad_stat_pairs.iter().enumerate() {
            let super_category = split_weights.binary_search_by(|&x| {
                if x < mean { Ordering::Less }
                else { Ordering::Greater }
            }).expect_err("in this universe, nothing is equal (see cmp impl above)");
            super_categories[category] = super_category as CatT;
            println!("category {:?} -> {:?} [mean {} < {:?}]", category, super_category, mean,
                     split_weights.get(super_category));
        }

        dbg!(&split_weights);
        dbg!(&super_categories);

        // generate bitvecs
        let iter = self.example_sel.iter().map(|&i| data[i]);
        let numt2cat = |x| super_categories[into_cat(x) as usize] as usize;
        let bitvecs = construct_bitvecs(&mut self.store, n, super_card, iter, numt2cat);
        transform_bitvecs_to_ord(&mut self.store, &bitvecs);

        self.bitvecs[feat_id] = bitvecs;
        self.super_categories[feat_id] = Rc::new(super_categories);
    }

    /// - Generate too many split value candidates using quantile estimates.
    /// - Treat the result as a high cardinality categorical 
    fn preprocess_num(&mut self, feat_id: usize) {
        let n = self.example_sel.len();
        let data = self.data.get_feature(feat_id);
        let lims = self.data.feat_limits(feat_id);
        let gradient = self.gradient;

        // quantile estimation, weighted by gradient values so there is variation in the limited
        // number of split candidates we generate
        self.bins_buffer_numt.iter_mut().for_each(|b| *b = 0.0);
        let mut binner = Binner::new(&mut self.bins_buffer_numt, lims);
        let mut grad_weight_sum = 0.0;
        let combiner = |bin: &mut NumT, d: NumT| *bin += d;
        for (x, t) in self.example_sel.iter().map(|&i| (data[i], gradient[i].abs() + EPSILON)) {
            // XXX Apply weight transformation?
            grad_weight_sum += t;
            binner.insert(x, t, combiner);
        }

        // extract approximate quantiles
        let weight_step = grad_weight_sum / (self.max_nbins + 1) as NumT;
        let weights = (1..=self.max_nbins).map(|i| i as NumT * weight_step);
        let qbins = binner.rank_iter(weights, |bin| *bin);
        let mut last_bin = usize::max_value();
        let mut split_values = Vec::with_capacity(self.max_nbins);
        for bin in qbins {
            if bin == last_bin { continue; }
            last_bin = bin;
            split_values.push(binner.bin_representative(bin));
        }

        dbg!(&split_values);

        // construct bitvecs
        let iter = self.example_sel.iter().map(|&i| data[i]);
        let numt2cat = |x| {
            split_values.binary_search_by(|&s| {
                if s < x { Ordering::Less }
                else     { Ordering::Greater }
            }).expect_err("in this universe, nothing is equal")
        };
        let bitvecs = construct_bitvecs(&mut self.store, n, split_values.len(), iter, numt2cat);
        transform_bitvecs_to_ord(&mut self.store, &bitvecs);

        self.bitvecs[feat_id] = bitvecs;
        self.split_values[feat_id] = split_values;
    }


    // ----------
    
    /// Depending on the gradient values, the number of bitvecs can vary slightly, with a maximum
    /// of 'max_nbins'. This method returns an upper bound of the nbins that is safe regardless of
    /// the used gradients.
    pub fn get_max_nbins(&self, feat_id: usize) -> usize {
        match self.data.feat_type(feat_id) {
            FeatType::LoCardCat => self.feat_card(feat_id),
            FeatType::HiCardCat => self.max_nbins,
            FeatType::Numerical => self.max_nbins,
        }
    }

    /// Get the number of bins actually used (may be less than get_max_nbins, depending on the
    /// target values).
    pub fn get_nbins(&self, feat_id: usize) -> usize {
        self.bitvecs[feat_id].len()
    }
    
    /// Get the binary representation for a specific split of a feature.
    pub fn get_bitvec(&self, feat_id: usize, split_id: usize) -> BitVecRef {
        let range = self.bitvecs[feat_id][split_id];
        self.store.get_bitvec(range)
    }

    /// Split value: for low-card cat,  tree splits check equality with this value.
    ///              for numerical,     tree splits check lt < with this value.
    ///              for high-card cat, different, compare with super-category! 
    pub fn get_split_value(&self, feat_id: usize, split_id: usize) -> NumT {
        match self.feat_type(feat_id) {
            FeatType::LoCardCat => split_id as NumT,
            FeatType::HiCardCat => split_id as NumT, // == super-category
            FeatType::Numerical => self.split_values[feat_id][split_id],
        }
    }

    /// Get the super-category (category of categories) of a value of a high-cardinality
    /// cateogrical feature.
    pub fn get_super_category(&self, feat_id: usize, value: NumT) -> CatT {
        debug_assert_eq!(self.feat_type(feat_id), FeatType::HiCardCat);
        self.super_categories[feat_id][into_cat(value) as usize]
    }

    pub fn nfeatures(&self) -> usize { self.feat_sel.len() }
    pub fn feat_ids(&self) -> &[usize] { &self.feat_sel }
    pub fn nexamples(&self) -> usize { self.example_sel.len() }
    pub fn examples(&self) -> &[usize] { &self.example_sel }
    pub fn feat_name(&self, feature: usize) -> &str { &self.data.names[feature] }
    pub fn feat_limits(&self, feat_id: usize) -> (NumT, NumT) { self.data.limits[feat_id] }
    pub fn feat_type(&self, feat_id: usize) -> FeatType { self.data.ftypes[feat_id] }
    pub fn feat_card(&self, feat_id: usize) -> usize { self.data.cards[feat_id] }
    pub fn get_feature(&self, feat_id: usize) -> &[NumT] { self.data.get_feature(feat_id) }
    pub fn get_target(&self) -> &[NumT] { self.data.get_target() }
    pub fn get_gradient(&self) -> &[NumT] { &self.gradient }
}







// ------------------------------------------------------------------------------------------------

fn sample(n: usize, buffer: &mut [usize], seed: u64) {
    use rand::{Rng, SeedableRng};
    use rand::rngs::SmallRng;

    let mut rng: SmallRng = SmallRng::seed_from_u64(seed);
    buffer.iter_mut().for_each(|i| *i = rng.gen_range(0, n));
    buffer.sort_unstable();
}

fn reservoir_sample(n: usize, buffer: &mut [usize], seed: u64) {
    use rand::{Rng, SeedableRng};
    use rand::rngs::SmallRng;

    let mut rng: SmallRng = SmallRng::seed_from_u64(seed);
    let k = buffer.len();

    for i in 0..n {
        if i < k { buffer[i] = i; }
        else {
            let r = rng.gen_range(0, i+1);
            if r < k { buffer[r] = i }
        }
    }
    buffer.sort_unstable();
}

fn construct_bitvecs<Iter, CatMap>(store: &mut BitBlockStore, nexamples: usize, card: usize,
                                   iter: Iter, numt2cat: CatMap)
    -> Vec<SliceRange>
where Iter: Iterator<Item=NumT>,
      CatMap: Fn(NumT) -> usize,
{
    let mut bitvecs = Vec::with_capacity(card);
    for _ in 0..card { bitvecs.push(store.alloc_zero_bits(nexamples)); }

    for (i, x) in iter.enumerate() {
        let category = (numt2cat)(x) as usize;
        if category < card {
            let mut bitvec = store.get_bitvec_mut(bitvecs[category]);
            bitvec.enable_bit(i);
        }
    }

    bitvecs
}

fn transform_bitvecs_to_ord(store: &mut BitBlockStore, bitvecs: &[SliceRange]) {
    for (&r0, &r1) in bitvecs[0..].iter().zip(bitvecs[1..].iter()) {
        let (bv0, mut bv1) = store.get_two_bitvecs_mut(r0, r1);
        unsafe { simd::or_assign(&mut bv1, &bv0); }
    }
}







// ------------------------------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use crate::config::Config;
    use super::*;

    #[test]
    fn basic_with_header() {
        let mut config = Config::new();
        config.csv_has_header = true;
        config.csv_delimiter = b';';
        config.categorical_features = vec![2];

        let data = Data::from_csv(&config, "a;bb;ccc;t\n1.0;2.0;0.0;0\n4;5;1;1\n\n").unwrap();
        let tindex = data.target_id();

        assert_eq!(data.nfeatures(), 3);
        assert_eq!(data.nexamples(), 2);
        assert_eq!(data.get_feature(0)[0], 1.0);
        assert_eq!(data.get_feature(1)[0], 2.0);
        assert_eq!(data.get_feature(2)[0], 0.0);
        assert_eq!(data.get_feature(0)[1], 4.0);
        assert_eq!(data.get_feature(1)[1], 5.0);
        assert_eq!(data.get_feature(2)[1], 1.0);
        assert_eq!(data.feat_name(0), "a");
        assert_eq!(data.feat_name(1), "bb");
        assert_eq!(data.feat_name(2), "ccc");
        assert_eq!(data.feat_limits(0), (1.0, 4.0));
        assert_eq!(data.feat_limits(1), (2.0, 5.0));
        assert_eq!(data.feat_limits(2), (0.0, 1.0));
        assert_eq!(data.feat_card(0), 0);
        assert_eq!(data.feat_card(1), 0);
        assert_eq!(data.feat_card(2), 2);
        assert_eq!(data.get_feature(tindex)[0], 0.0);
        assert_eq!(data.get_feature(tindex)[1], 1.0);
    }

    #[test]
    fn basic_without_header() {
        let mut config = Config::new();
        config.csv_has_header = false;

        let data = Data::from_csv(&config, "1.0,2.0,3.0,0\n4,5,6,0\n").unwrap();

        assert_eq!(data.nfeatures(), 3);
        assert_eq!(data.nexamples(), 2);
        assert_eq!(data.get_feature(0)[0], 1.0);
        assert_eq!(data.get_feature(1)[0], 2.0);
        assert_eq!(data.get_feature(2)[0], 3.0);
        assert_eq!(data.get_feature(0)[1], 4.0);
        assert_eq!(data.get_feature(1)[1], 5.0);
        assert_eq!(data.get_feature(2)[1], 6.0);
        assert_eq!(data.feat_name(0), "");
        assert_eq!(data.feat_name(1), "");
        assert_eq!(data.feat_name(2), "");
        assert_eq!(data.feat_limits(0), (1.0, 4.0));
        assert_eq!(data.feat_limits(1), (2.0, 5.0));
        assert_eq!(data.feat_limits(2), (3.0, 6.0));
    }
    
    #[test]
    fn basic_dataset() {
        let mut config = Config::new();
        config.csv_has_header = false;
        config.random_seed = 2;
        config.feature_fraction = 0.67;
        config.example_fraction = 0.75;
        config.categorical_features = vec![1];

        let d = "1,0,3,0\n4,1,6,0\n7,1,9,0\n10,1,12,0\n13,0,15,0\n16,2,18,0\n19,2,21,0\n22,0,24,0";
        let data = Data::from_csv(&config, d).unwrap();

        assert_eq!(data.nexamples(), 8);
        assert_eq!(data.feat_card(1), 3);

        let target = data.get_feature(data.target_id());
        let dataset = Dataset::construct_from_data(&config, &data, target);

        assert_eq!(dataset.feat_sel.len(), 2);
        assert_eq!(dataset.example_sel.len(), 6);
        assert_eq!(&dataset.feat_sel, &[1, 2]);
        assert_eq!(&dataset.example_sel, &[0, 2, 2, 5, 7, 7]);

        let ranges = &dataset.bitvecs[1];
        let values = vec![0b110001, 0b000110, 0b001000];
        for i in 0..3 {
            let bitvec = dataset.store.get_bitvec(ranges[i]);
            let x = bitvec.cast::<u32>()[0];
            println!("{:3}: {:032b}", i, x);
            assert_eq!(values[i], x);
        }
    }

    #[test]
    fn dataset_hicard_cat() {
        let mut config = Config::new();
        config.csv_has_header = false;
        config.categorical_features = vec![0];
        config.max_nbins = 8;
        let d = "1,1\n1,1\n2,1\n2,1\n3,2\n3,2\n4,2\n4,2\n5,3\n5,3\n6,3\n6,3\n7,4\n7,4\n8,4\n8,4\n\
                 9,5\n9,5\n10,5\n10,5\n11,6\n11,6\n12,6\n12,6\n13,7\n13,7\n14,7\n14,7\n15,8\n15,8\
                 \n16,8\n16,8";
        let data = Data::from_csv(&config, d).unwrap();
        let dataset = Dataset::construct_from_data(&config, &data, data.get_target());

        let ranges = &dataset.bitvecs[0];
        let values = vec![0b00000000000000000000000000001111u32,
                          0b00000000000000000000000011111111,
                          0b00000000000000000000111111111111,
                          0b00000000000000001111111111111111,
                          0b00000000000011111111111111111111,
                          0b00000000111111111111111111111111,
                          0b00001111111111111111111111111111];
        for (i, &r) in ranges.iter().enumerate() {
            let bitvec = dataset.store.get_bitvec(r);
            let x = bitvec.cast::<u32>()[0];
            println!("{:3}: {:032b}", i, x);
            assert_eq!(values[i], x);
        }
    }

    #[test]
    fn dataset_nbins() {
        let mut config = Config::new();
        config.csv_has_header = false;
        config.categorical_features = vec![0];
        config.max_nbins = 8;
        let d = "8,1\n7,1\n1,0\n7,1\n3,0\n8,1\n6,1\n2,0\n5,1\n4,1\n2,0\n7,1\n3,0\n8,1\n6,1\n3,0\n\
                 7,1\n5,1\n5,1\n4,1\n2,0\n1,0\n6,1\n2,0\n6,1\n1,0\n4,1\n3,0\n4,1\n8,1\n1,0\n5,1";
        let data = Data::from_csv(&config, d).unwrap();
        let dataset = Dataset::construct_from_data(&config, &data, data.get_target());

        assert_eq!(8, dataset.get_max_nbins(0));
        assert_eq!(2, dataset.get_nbins(0));
    }

    fn dataset_num_aux(data_str: &str, values: &[u32]) {
        let mut config = Config::new();
        config.csv_has_header = false;
        config.max_nbins = 8;
        let data = Data::from_csv(&config, data_str).unwrap();
        let dataset = Dataset::construct_from_data(&config, &data, data.get_target());

        dbg!(&data.features);

        let ranges = &dataset.bitvecs[0];
        for (i, &r) in ranges.iter().enumerate() {
            let bitvec = dataset.store.get_bitvec(r);
            let x = bitvec.cast::<u32>()[0];
            println!("{:3}: {:032b}", i, x);
            assert_eq!(values[i], x);
        }
    }

    #[test]
    fn dataset_num1() {
        let values = vec![0b00000000000000000000000000000011u32,
                          0b00000000000000000000000001111111,
                          0b00000000000000000000001111111111,
                          0b00000000000000000011111111111111,
                          0b00000000000000011111111111111111,
                          0b00000000000111111111111111111111,
                          0b00000000111111111111111111111111,
                          0b00000111111111111111111111111111];
        let d = "0,1\n6,1\n11,1\n11,1\n13,1\n21,1\n24,1\n31,1\n36,1\n38,1\n42,1\n48,1\n60,1\n60,1\
                 \n61,1\n61,1\n64,1\n68,1\n75,1\n80,1\n81,1\n84,1\n85,1\n86,1\n89,1\n90,1\n91,1\n\
                 92,1\n92,1\n93,1\n96,1\n98,1";
        dataset_num_aux(d, &values);
    }

    #[test]
    fn dataset_num2() {
        let values = vec![0b00000000000000000000001111111111u32, // less weight
                          0b00000000000000000011111111111111,
                          0b00000000000000111111111111111111,
                          0b00000000000111111111111111111111,
                          0b00000000011111111111111111111111,
                          0b00000011111111111111111111111111,
                          0b00000111111111111111111111111111,
                          0b00111111111111111111111111111111]; // more weight -> finer splits
        let d = "0,1\n6,2\n11,3\n11,4\n13,5\n21,7\n24,8\n31,9\n36,10\n38,11\n42,12\n48,13\n60,14\
                 \n60,15\n61,16\n61,18\n64,19\n68,20\n75,21\n80,22\n81,23\n84,24\n85,25\n86,26\n\
                 89,27\n90,28\n91,30\n92,31\n92,32\n93,33\n96,34\n98,35";
        dataset_num_aux(d, &values);
    }

    #[test]
    fn dataset_all() {
        let mut config = Config::new();
        config.categorical_features = vec![0, 1];
        config.csv_has_header = false;
        config.max_nbins = 8;
        let d = "6,16,1,0.01\n4,19,2,0.02\n5,6,3,0.02\n0,4,4,0.03\n6,5,5,0.03\n4,4,6,0.04\n1,15,7,0.08\n2,16,8,0.09\n6,8,9,0.09\n4,14,10,0.09\n2,2,11,0.1\n5,11,12,0.13\n4,1,13,0.14\n0,9,14,0.18\n0,18,15,0.22\n3,12,16,0.22\n1,18,17,0.24\n0,8,18,0.27\n6,17,19,0.28\n3,14,20,0.28\n0,12,21,0.3\n6,16,22,0.32\n5,1,23,0.35\n0,13,24,0.36\n6,17,25,0.37\n3,10,26,0.37\n2,3,27,0.38\n6,9,28,0.4\n1,18,29,0.44\n5,7,30,0.45\n2,4,31,0.45\n6,5,32,0.49\n0,14,33,0.49\n2,19,34,0.49\n1,20,35,0.5\n4,3,36,0.53\n3,9,37,0.54\n6,20,38,0.6\n2,12,39,0.61\n6,11,40,0.62\n2,6,41,0.63\n0,8,42,0.65\n3,19,43,0.68\n4,13,44,0.7\n4,15,45,0.71\n5,2,46,0.74\n5,10,47,0.74\n6,3,48,0.75\n6,7,49,0.76\n6,15,50,0.76\n3,11,51,0.77\n5,2,52,0.8\n6,1,53,0.82\n2,7,54,0.84\n1,4,55,0.86\n6,13,56,0.88\n3,5,57,0.89\n3,20,58,0.92\n5,6,59,0.92\n1,1,60,0.94\n4,2,61,0.96\n6,17,62,0.99\n1,3,63,0.99\n1,10,64,0.99";
        let data = Data::from_csv(&config, d).unwrap();
        let dataset = Dataset::construct_from_data(&config, &data, data.get_target());

        assert_eq!(dataset.get_max_nbins(0), 7);
        assert_eq!(dataset.get_max_nbins(1), 8);
        assert_eq!(dataset.get_max_nbins(2), 8);
        assert_eq!(dataset.get_nbins(0), 7);
        assert_eq!(dataset.get_nbins(1), 8);
        assert_eq!(dataset.get_nbins(2), 8);

        let values = vec![0b0000000000000000000000100000000100000000100100100110000000001000,
                          0b1100100001000000000000000000010000010000000000010000000001000000,
                          0b0000000000100000000000010100001001000100000000000000010010000000,
                          0b0000001100000100000001000001000000000010000010001000000000000000,
                          0b0001000000000000000110000000100000000000000000000001001000100010,
                          0b0000010000001000011000000000000000100000010000000000100000000100,
                          0b0010000010010011100000001010000010001001001001000000000100010001,
                          0b0, // skip
                          
                          0b0000000000000000000000000000000100000000001010000000001010000001,
                          0b0000000000000000000000100000000100010000001010110100001110000001,
                          0b0000000001000000000000100001000101011000001010110110001110101001,
                          0b0000000001000000000001100101001101011000001110111110001110101011,
                          0b0000000101000110000101101101001111011000001110111110101111111011,
                          0b0010010101000110000101111101001111011001001111111110101111111111,
                          0b0010110111010110000111111101001111011001111111111111101111111111,
                          0b0111110111011110101111111101101111011101111111111111111111111111,
                          
                          0b0000000000000000000000000000000000000000001111111111111111111111,
                          0b0000000000000000000000000000000001111111111111111111111111111111,
                          0b0000000000000000000000000011111111111111111111111111111111111111,
                          0b0000000000000000000001111111111111111111111111111111111111111111,
                          0b0000000000000000111111111111111111111111111111111111111111111111,
                          0b0000000000001111111111111111111111111111111111111111111111111111,
                          0b0000000011111111111111111111111111111111111111111111111111111111,
                          0b0000111111111111111111111111111111111111111111111111111111111111u64];

        for k in 0..3 {
            println!("== feature {}", k);
            let ranges = &dataset.bitvecs[k];
            for (i, &r) in ranges.iter().enumerate() {
                let bitvec = dataset.store.get_bitvec(r);
                let x = bitvec.cast::<u64>()[0];
                println!("{:3}: {:064b}", i, x);
                assert_eq!(values[k * 8 + i], x);
            }
            println!();
        }
    }
}
