// includes.rs
pub use std::{any::Any, collections::HashSet, fs, mem::replace};

pub use arrow2::{
    array::{Array, BooleanArray, PrimitiveArray},
    chunk::Chunk,
    compute::{arithmetics, boolean, comparison},
    datatypes::{DataType, Field, Schema},
    error::Result as A2Result,
};
pub use serde::{Deserialize, Serialize};
pub use tracing::{debug, error, event, info, Level};
pub use crate::graph::KeyId;

pub use crate::{
    datum::{Datum, Datum::*},
    env::Env,
    stage::StageLink,
    *,
};

pub const TOPDIR: &str = "/Users/adarshrp/Projects/kona";
pub const TEMPDIR: &str = "/Users/adarshrp/Projects/kona/tmp";

pub type ColId = usize;
pub type QunId = usize;
pub type QBId = usize;
pub type PartitionId = usize;
pub type StageId = usize;

#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QunCol(pub QunId, pub ColId);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TextFilePartition(pub u64, pub u64);

pub fn enquote(s: &String) -> String {
    format!("\"{}\"", s)
}

pub fn remove_quotes(s: &str) -> String {
    s.replace('"', "")
}

pub fn stringify<E: std::fmt::Debug>(e: E) -> String {
    format!("{:?}", e)
}

pub fn stringify1<E: std::fmt::Debug, P: std::fmt::Debug>(e: E, param1: P) -> String {
    format!("{:?}: {:?}", e, param1)
}

pub fn yes_or_no(s: &str) -> Option<bool> {
    match s {
        "Y" | "YES" => Some(true),
        "N" | "NO" => Some(false),
        _ => None,
    }
}

macro_rules! fprint {
    ($file:expr, $($args:expr),*) => {{
        $file.write_all(format!($($args),*).as_bytes()).map_err(stringify)?;
    }};
}
pub(crate) use fprint;

pub fn list_files(dirname: &String) -> Result<Vec<String>, String> {
    let dir = fs::read_dir(dirname).map_err(|err| stringify1(err, dirname))?;
    let mut pathnames = vec![];
    for entry in dir {
        let entry = entry.map_err(stringify)?;
        let path = entry.path();
        if !path.is_dir() {
            let pathstr = path.into_os_string().into_string().map_err(stringify)?;
            pathnames.push(pathstr)
        }
    }
    Ok(pathnames)
}

pub type ChunkBox = Chunk<Box<dyn Array>>;

pub const CHUNK_SIZE: usize = 1024;

#[allow(unused_macros)]
macro_rules! function_name {
    () => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        let name = type_name_of(f);
        &name[..name.len() - 3]
    }};
}

#[allow(unused_imports)]
pub(crate) use function_name;

pub fn get_partition_dir(flow_id: usize, stage_link: StageLink, pid: PartitionId) -> String {
    format!("{}/flow-{}/pipeline-{}-{}/consumer-{}", TEMPDIR, flow_id, stage_link.0, stage_link.1, pid)
}

pub fn get_output_dir(flow_id: usize) -> String {
    format!("{}/flow-{}/output", TEMPDIR, flow_id)
}

pub fn has_duplicates<T: Eq + std::hash::Hash>(vec: &[T]) -> bool {
    // Create a HashSet from the vector to remove duplicates
    let set: HashSet<&T> = vec.iter().collect();

    // Compare the lengths to check for duplicates
    vec.len() != set.len()
}
