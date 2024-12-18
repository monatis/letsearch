use super::collection_utils::SearchResult;
use crate::collection::collection_utils::{home_dir, CollectionConfig};
use crate::collection::vector_index::VectorIndex;
use crate::model::model_manager::ModelManager;
use crate::model::model_utils::{Embeddings, ModelOutputDType};
use anyhow::Error;
use duckdb::arrow::array::{PrimitiveArray, StringArray};
use duckdb::arrow::datatypes::UInt64Type;
use duckdb::arrow::record_batch::RecordBatch;
use duckdb::Connection;
use log::{debug, info};
use serde_json;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use usearch::f16 as UsearchF16;
use usearch::{IndexOptions, MetricKind, ScalarKind};

pub struct Collection {
    config: CollectionConfig,
    // TODO: is it really necessary to acquire a lock on this? duckdb seems to be thread-safe itself.
    conn: Arc<RwLock<Connection>>,
    vector_index: RwLock<HashMap<String, Arc<RwLock<VectorIndex>>>>,
}

impl Collection {
    pub async fn new(config: CollectionConfig, overwrite: bool) -> anyhow::Result<Self> {
        debug!("creating new Collection instance");
        let name = config.name.as_str();
        let collection_dir = home_dir().join("collections").join(name);
        let collection_dir_str = collection_dir.to_str().unwrap();
        if overwrite && collection_dir.exists() {
            debug!("Collection already exists, overwriting");
            fs::remove_dir_all(collection_dir_str)?;
            debug!("removed existing collection for overwriting");
        }

        fs::create_dir_all(collection_dir_str)?;
        debug!("Created collection dir: {collection_dir_str}");
        let db_path = collection_dir.join(config.db_path.as_str());

        let conn = Connection::open(db_path).expect("error while trying to open connection to db");
        debug!("Connection opened to DB");

        let config_file = File::create(collection_dir.join("config.json").to_str().unwrap())
            .expect("error while trying to create config.json");
        let _ = serde_json::to_writer(config_file, &config).unwrap();

        Ok(Collection {
            config: config,
            conn: Arc::new(RwLock::new(conn)),
            vector_index: RwLock::new(HashMap::new()),
        })
    }

    pub async fn from(name: String) -> anyhow::Result<Self> {
        let collection_dir = home_dir().join("collections").join(name.as_str());
        if !collection_dir.exists() {
            return Err(Error::msg("Collection {name} does not exist"));
        }

        let config_path = collection_dir.join("config.json");
        if !config_path.exists() {
            return Err(Error::msg("config file does not exist"));
        }

        let config_file = File::open(config_path).unwrap();
        let config: CollectionConfig = serde_json::from_reader(config_file)?;
        let conn = Connection::open(collection_dir.join(config.db_path.as_str()))?;

        let vector_indexes = RwLock::new(HashMap::new());
        let index_dir = collection_dir.join(config.index_dir.as_str());
        if index_dir.exists() && !config.index_columns.is_empty() {
            {
                let mut indexes_guard = vector_indexes.write().await;
                for index_column in config.index_columns.clone() {
                    let index_path = index_dir.join(index_column.as_str());
                    let vector_index = VectorIndex::from(index_path.to_path_buf())?;

                    indexes_guard.insert(index_column.clone(), Arc::new(RwLock::new(vector_index)));
                }
            }
        }

        Ok(Collection {
            config: config,
            conn: Arc::new(RwLock::new(conn)),
            vector_index: vector_indexes,
        })
    }

    pub fn config(&self) -> CollectionConfig {
        self.config.clone()
    }

    pub async fn import_jsonl(&self, jsonl_path: &str) -> anyhow::Result<()> {
        let start = Instant::now();
        // prevent deadlock when add_keys_to_db is trying to acquire a lock
        {
            let conn = self.conn.clone();
            let mut conn_guard = conn.write().await;
            let tx = conn_guard.transaction()?;
            tx.execute_batch(
                format!(
                    "CREATE TABLE {} AS SELECT * FROM read_json_auto('{}');",
                    &self.config.name, jsonl_path
                )
                .as_str(),
            )?;
            self.add_keys_to_db(&tx).await?;

            tx.commit()?;
        }

        info!(
            "Records imported from {:?} in {:?}",
            jsonl_path,
            start.elapsed()
        );

        Ok(())
    }

    pub async fn import_parquet(&self, parquet_path: &str) -> anyhow::Result<()> {
        let start = Instant::now();
        // prevent deadlock when add_keys_to_db is trying to acquire a lock
        {
            let conn = self.conn.clone();
            let mut conn_guard = conn.write().await;
            let tx = conn_guard.transaction()?;

            tx.execute_batch(
                format!(
                    "CREATE TABLE {} AS SELECT * FROM read_parquet('{}', filename = true);",
                    &self.config.name, parquet_path
                )
                .as_str(),
            )?;
            self.add_keys_to_db(&tx).await?;
            tx.commit()?;
        }

        info!(
            "Records imported from {:?} in {:?}",
            parquet_path,
            start.elapsed()
        );

        Ok(())
    }

    pub async fn get_single_column(
        &self,
        column_name: &str,
        limit: u64,
        offset: u64,
        keys: Vec<u64>,
    ) -> anyhow::Result<Vec<String>> {
        assert!(limit >= 1);
        let conn = self.conn.clone();
        let conn_guard = conn.read().await;
        let query = if keys.is_empty() {
            format!(
                "SELECT {} FROM {} LIMIT {} OFFSET {};",
                column_name, &self.config.name, limit, offset
            )
        } else {
            let keys_str = keys
                .iter()
                .map(|key| key.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "SELECT {} FROM {} WHERE _key IN ({}) LIMIT {} OFFSET {};",
                column_name, &self.config.name, &keys_str, limit, offset,
            )
        };

        let mut stmt = conn_guard.prepare(&query)?;
        let result: Vec<RecordBatch> = stmt.query_arrow([])?.collect();
        assert_eq!(result.len(), 1);
        let batch = &result[0];

        let col_array = batch
            .column_by_name(column_name)
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let col_values: Vec<String> = col_array
            .iter()
            .map(|s| s.unwrap().to_string())
            .collect::<Vec<String>>();

        Ok(col_values)
    }

    async fn embed_column_with_offset(
        &mut self,
        column_name: &str,
        batch_size: u64,
        offset: u64,
        model_manager: Arc<RwLock<ModelManager>>,
        model_id: u32,
    ) -> anyhow::Result<()> {
        let start = Instant::now();
        let (texts, keys) = self
            .get_column_and_keys(column_name, batch_size, offset)
            .await?;
        debug!("getting texts from DB took: {:?}", start.elapsed());
        let start = Instant::now();
        let inputs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let embeddings = model_manager
            .read()
            .await
            .predict(model_id, inputs)
            .await
            .unwrap();

        match embeddings {
            Embeddings::F16(emb) => {
                let (_, vector_dim) = emb.dim();

                let indexes_guard = self.vector_index.read().await;
                let index = indexes_guard.get(column_name).unwrap().clone();
                let index_guard = index.write().await;
                index_guard
                    .add::<UsearchF16>(&keys, emb.as_ptr() as *const UsearchF16, vector_dim)
                    .await
                    .unwrap();
            }
            Embeddings::F32(emb) => {
                let (_, vector_dim) = emb.dim();

                let indexes_guard = self.vector_index.read().await;
                let index = indexes_guard.get(column_name).unwrap().clone();
                let index_guard = index.write().await;
                index_guard
                    .add::<f32>(&keys, emb.as_ptr(), vector_dim)
                    .await
                    .unwrap();

                debug!("output shape: {:?}", emb.dim());
            }
        }

        debug!("Embedding texts took: {:?}", start.elapsed());
        Ok(())
    }

    pub async fn embed_column(
        &mut self,
        column_name: &str,
        batch_size: u64,
        model_manager: Arc<RwLock<ModelManager>>,
        model_id: u32,
    ) -> anyhow::Result<()> {
        let count: u64 = {
            let conn_guard = self.conn.read().await;
            let query = format!("SELECT COUNT('{}') FROM {};", column_name, self.config.name);
            let mut stmt = conn_guard.prepare(&query)?;
            let count: i64 = stmt.query_row([], |row| row.get(0))?;

            count as u64
        };
        let num_batches = (count + batch_size - 1) / batch_size;
        info!("Starting to index {count} records from column '{column_name}' in batches of {batch_size}");

        {
            let mut indexes_guard = self.vector_index.write().await;
            if !indexes_guard.contains_key(column_name) {
                let vector_dim = model_manager
                    .read()
                    .await
                    .output_dim(model_id)
                    .await
                    .unwrap();
                let output_dtype = model_manager
                    .read()
                    .await
                    .output_dtype(model_id)
                    .await
                    .unwrap();
                let scalar_kind = match output_dtype {
                    ModelOutputDType::F32 => ScalarKind::F32,
                    ModelOutputDType::F16 => ScalarKind::F16,
                    ModelOutputDType::Int8 => ScalarKind::I8,
                };

                let index_path = home_dir()
                    .join("collections")
                    .join(self.config.name.as_str())
                    .join(self.config.index_dir.as_str())
                    .join(column_name);
                let options = IndexOptions {
                    dimensions: vector_dim as usize,
                    metric: MetricKind::Cos,
                    quantization: scalar_kind,
                    connectivity: 0,
                    expansion_add: 0,
                    expansion_search: 0,
                    multi: true,
                };
                let mut index = VectorIndex::new(index_path, true).unwrap();
                index.with_options(&options, 20000).unwrap();
                indexes_guard.insert(column_name.to_string(), Arc::new(RwLock::new(index)));
            }
        }

        let start = Instant::now();

        for batch in 0..num_batches {
            let elapsed = start.elapsed();
            let steps_completed = batch as f64;
            let total_steps = num_batches as f64;
            let eta = if steps_completed > 0.0 {
                elapsed.mul_f64((total_steps - steps_completed) / steps_completed)
            } else {
                Duration::ZERO
            };

            // Format ETA as seconds

            // print progress
            print!("\r{} / {} batches - ETA: {:?}", batch, total_steps, eta);
            std::io::Write::flush(&mut std::io::stdout()).unwrap();

            self.embed_column_with_offset(
                column_name,
                batch_size,
                batch * batch_size,
                model_manager.clone(),
                model_id,
            )
            .await
            .unwrap();
        }

        // save index to disk
        self.vector_index
            .read()
            .await
            .clone()
            .get(column_name)
            .unwrap()
            .read()
            .await
            .save()
            .unwrap();

        println!("");
        info!("Total duration: {:?}", start.elapsed());

        Ok(())
    }

    pub async fn requested_models(&self) -> Vec<(String, String)> {
        vec![(
            self.config.model_name.clone(),
            self.config.model_variant.clone(),
        )]
    }

    pub async fn search(
        &self,
        column_name: String,
        query: String,
        limit: u32,
        model_manager: Arc<RwLock<ModelManager>>,
        model_id: u32,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let texts = vec![query.as_str()];
        let embeddings = model_manager.read().await.predict(model_id, texts).await?;

        let similarity_results = match embeddings {
            Embeddings::F16(emb) => {
                let (_, vector_dim) = emb.dim();

                self.vector_index
                    .read()
                    .await
                    .get(column_name.as_str())
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Index not found for {}", column_name))?
                    .read()
                    .await
                    .search::<UsearchF16>(
                        emb.as_ptr() as *const UsearchF16,
                        vector_dim,
                        limit as usize,
                    )
                    .await?
            }
            Embeddings::F32(emb) => {
                let (_, vector_dim) = emb.dim();

                self.vector_index
                    .read()
                    .await
                    .get(column_name.as_str())
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Index not found for {}", column_name))?
                    .read()
                    .await
                    .search::<f32>(emb.as_ptr(), vector_dim, limit as usize)
                    .await?
            }
        };

        let similar_keys: Vec<u64> = similarity_results.iter().map(|r| r.key).collect();
        let contents = self
            .get_single_column(
                column_name.as_str(),
                similar_keys.len() as u64,
                0,
                similar_keys,
            )
            .await?;

        let search_results = similarity_results
            .iter()
            .zip(contents.iter())
            .map(|(result, content)| SearchResult {
                content: content.to_string(),
                key: result.key,
                score: result.score,
            })
            .collect();

        Ok(search_results)
    }

    async fn add_keys_to_db(&self, tx: &duckdb::Transaction<'_>) -> anyhow::Result<()> {
        //let conn = self.conn.clone();
        //let conn_guard = conn.read().await;

        // Check if the '_key' column exists in the table
        let query = format!(
            "SELECT COUNT(*) FROM information_schema.columns WHERE table_name = '{}' AND column_name = '_key';",
            self.config.name
        );
        let exists: bool = {
            let mut stmt = tx.prepare(&query)?;
            let count: i64 = stmt.query_row([], |row| row.get(0))?;
            count > 0
        };

        if !exists {
            tx.execute_batch(
                format!(
                    r"CREATE SEQUENCE keys_seq;
    ALTER TABLE {} ADD COLUMN _key UBIGINT DEFAULT NEXTVAL('keys_seq');
    ",
                    self.config.name,
                )
                .as_str(),
            )?;
        }

        Ok(())
    }

    pub async fn get_column_and_keys(
        &self,
        column_name: &str,
        limit: u64,
        offset: u64,
    ) -> anyhow::Result<(Vec<String>, Vec<u64>)> {
        assert!(limit >= 1);
        let conn = self.conn.clone();
        let conn_guard = conn.read().await;

        // Query the specified column and `_key` together
        let mut stmt = conn_guard.prepare(
            format!(
                "SELECT {}, _key FROM {} LIMIT {} OFFSET {};",
                column_name, &self.config.name, limit, offset
            )
            .as_str(),
        )?;

        let result: Vec<RecordBatch> = stmt.query_arrow([])?.collect();
        assert_eq!(result.len(), 1);
        let batch = &result[0];

        // Extract the specified column values
        let col_array = batch
            .column_by_name(column_name)
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let col_values: Vec<String> = col_array
            .iter()
            .map(|s| s.unwrap().to_string())
            .collect::<Vec<String>>();

        // Extract `_key` values
        let key_array = batch
            .column_by_name("_key")
            .unwrap()
            .as_any()
            .downcast_ref::<PrimitiveArray<UInt64Type>>()
            .unwrap();
        let keys: Vec<u64> = key_array.iter().map(|key| key.unwrap_or(0)).collect();

        Ok((col_values, keys))
    }
}

// Needed because Rust does not understand Collection::conn is managed for thread safety.
unsafe impl Send for Collection {}
unsafe impl Sync for Collection {}
