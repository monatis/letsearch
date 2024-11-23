mod collection;
mod model;
mod serve;
use crate::collection::Collection;
use crate::model::manager::ModelManager;
use crate::model::model_traits::Backend;
use crate::serve::run_server;
use anyhow;
use chrono;
use clap::{Parser, Subcommand};
use env_logger::fmt::Formatter;
use log::{info, Record};
use std::io::Write;

/// CLI application for indexing and searching documents
#[derive(Parser, Debug)]
#[command(
    name = "letsearche",
    version = "0.1.0",
    author = "yusufsarigoz@gmail.com",
    about = "Index and search your documents, and serve it if you wish",
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Index documents
    Index {
        /// Path to files to index
        #[arg(required = true, num_args(1..), action = clap::ArgAction::Append)]
        files: Vec<String>,

        /// name of the collection to be created
        #[arg(short, long, required = true)]
        collection_name: String,

        /// Model to create embeddings
        #[arg(short, long, default_value = "bge-m3")]
        model: String,

        /// columns to embed and index for vector search
        #[arg(short, long, action = clap::ArgAction::Append)]
        index_columns: Vec<String>,

        /// remove and re-create collection directory if it exists
        #[arg(long, action=clap::ArgAction::SetTrue)]
        overwrite: bool,
    },

    /// serve a collection for search over web API
    Serve {
        /// host to listen to
        #[arg(short('H'), long, default_value = "127.0.0.1")]
        host: String,

        /// port to listen to
        #[arg(short, long, default_value = "7898")]
        port: i32,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .format(|buf: &mut Formatter, record: &Record| {
            writeln!(
                buf,
                "[{} {}] {}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                record.level(),
                record.args()
            )
        })
        .filter_module("ort::execution_providers", log::LevelFilter::Error)
        .filter_level(log::LevelFilter::Info)
        .init();

    let cli = Cli::parse(); // Automatically parses the arguments into the struct

    match &cli.command {
        Commands::Index {
            files,
            collection_name,
            model,
            index_columns,
            overwrite,
        } => {
            if !index_columns.is_empty() {
                info!("index columns: {:?}", index_columns);
            }
            let collection =
                Collection::new(collection_name.to_string(), overwrite.to_owned()).unwrap();
            let jsonl_path = &files[0];
            collection.import_jsonl(jsonl_path)?;
            let model_manager = ModelManager::new();
            let model_id = model_manager
                .load_model(model.to_string(), Backend::ONNX)
                .await
                .unwrap();
            info!("model successfully loaded from {model}");
            let _ = collection
                .embed_column("user", 2, 0, &model_manager, model_id)
                .await
                .unwrap();
        }

        Commands::Serve { host, port } => {
            run_server(host.to_string(), port.to_owned()).await.unwrap();
        }
    }

    Ok(())
}
