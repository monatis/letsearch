use anyhow;
use log::{debug, info};
use serde::Serialize;
use std::path::PathBuf;
use std::{fs, u64, usize};
use usearch::{new_index, Index, IndexOptions};

#[derive(Serialize)]
pub struct SimilarityResult {
    key: u64,
    score: f32,
}
pub struct VectorIndex {
    pub index: Option<Index>,
    path: PathBuf,
}

impl VectorIndex {
    pub fn new(index_dir: PathBuf, overwrite: bool) -> anyhow::Result<Self> {
        debug!("creating new VectorIndex instance");
        let index_dir_str = index_dir.to_str().unwrap();
        if overwrite && index_dir.exists() {
            debug!("index already exists, overwriting");
            fs::remove_dir_all(index_dir_str)?;
        }

        fs::create_dir_all(index_dir_str)?;

        Ok(VectorIndex {
            index: None,
            path: index_dir,
        })
    }

    pub fn with_options(
        &mut self,
        options: &IndexOptions,
        capacity: usize,
    ) -> anyhow::Result<&Self> {
        let index = new_index(options).unwrap();
        index.reserve(capacity).unwrap();
        self.index = Some(index);
        Ok(self)
    }

    pub fn from(path: PathBuf) -> anyhow::Result<Self> {
        let index_path = path.join("index.bin");
        let index_path_str = index_path.to_str().unwrap();
        info!("Index path: {:?}", index_path_str);
        let config = IndexOptions::default();
        let index = Index::new(&config)?;
        index.load(index_path_str)?;
        info!("vector index loaded from {:?}", path.to_str().unwrap());
        info!("vector count: {:?}", index.size());
        info!("vector dimensions: {:?}", index.dimensions());

        Ok(VectorIndex {
            index: Some(index),
            path: path,
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let index = self.index.as_ref().unwrap();
        let index_path = self.path.join("index.bin");
        index.save(index_path.to_str().unwrap()).unwrap();
        Ok(())
    }

    pub async fn add(
        &self,
        keys: &Vec<u64>,
        vectors: *const f32,
        vector_dim: usize,
    ) -> anyhow::Result<()> {
        let index = self.index.as_ref().unwrap();

        // TODO: parallelize with tokio_stream later on
        keys.iter().enumerate().for_each(|(i, _key)| {
            let vector_offset = unsafe { vectors.add(i * vector_dim) };
            let vector: &[f32] = unsafe { std::slice::from_raw_parts(vector_offset, vector_dim) };
            index.add(keys[i], vector).unwrap();
        });

        Ok(())
    }

    pub async fn search(
        &self,
        vector: *const f32,
        vector_dim: usize,
        count: usize,
    ) -> anyhow::Result<Vec<SimilarityResult>> {
        let query_vector: &[f32] = unsafe { std::slice::from_raw_parts(vector, vector_dim) };
        let index = self.index.as_ref().unwrap();
        let matches = index.search(query_vector, count).unwrap();
        let results: Vec<SimilarityResult> = matches
            .keys
            .iter()
            .zip(matches.distances.iter())
            .map(|(key, distance)| SimilarityResult {
                key: *key,
                score: 1.0 - distance,
            })
            .collect();

        Ok(results)
    }
}
