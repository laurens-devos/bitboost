macro_rules! try_or_str {
    ($result:expr, $($arg:tt)*) => {{
        match $result {
            Ok(x) => x,
            Err(_) => return Err(format!($($arg)*))
        }
    }}
}

pub type NumT = f32; // numeric type
pub type CatT = u32; // unsigned int of same size as NumT
pub const EPSILON: NumT = std::f32::EPSILON;
pub const POS_INF: NumT = std::f32::INFINITY;
pub const NEG_INF: NumT = std::f32::NEG_INFINITY;
pub fn into_cat(x: NumT) -> CatT { debug_assert!(x >= 0.0 && x.round() == x); x as CatT }

pub mod config;
pub mod dataset;
pub mod data;
pub mod bitblock;
pub mod simd;
pub mod slice_store;
pub mod tree;
pub mod tree_learner;
pub mod objective;
pub mod binner;
pub mod new_binner;
pub mod numfeat_mask_store;
pub mod metric;
pub mod boost;
