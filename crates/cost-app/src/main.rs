//! `wist-cost` 命令行（W0）：`migrate` / `ingest` / `healthz`。
//!
//! 三个入口都是**本地验证入口**，不实现查询 API（设计 §6：field 页面与授权轴暂缓）。
//! 所有输出都是 JSON，方便脚本与后续闸门比对。

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use cost_core::objects::{ObjectStore, ShardKeyLayout};
use cost_core::verify::verify_batch;
use cost_core::{Manifest, RULE_VERSION};
use cost_store::{Ingestor, ObjectStoreConfig, PgStore};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(
    name = "wist-cost",
    version,
    about = "wist-cost：成本域范围替换入账与自述（W0）"
)]
struct Cli {
    /// PG 连接串。缺省读 WIST_COST_DATABASE_URL / DATABASE_URL。
    #[arg(long, global = true, env = "WIST_COST_DATABASE_URL")]
    database_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 应用 `migrations/`（幂等）。
    Migrate,

    /// 按 manifest 入账一个成本范围；同一范围重复发布 = 整段替换。
    Ingest {
        /// manifest.json 路径。
        #[arg(long)]
        manifest: PathBuf,

        /// local 后端根目录（env: WIST_COST_OBJECT_ROOT，默认 "."）。
        #[arg(long, env = "WIST_COST_OBJECT_ROOT")]
        objects_root: Option<PathBuf>,

        /// 对象键模板（必须含 {shard_id}）。默认走设计 §4 的布局。
        #[arg(long, env = "WIST_COST_OBJECT_KEY_TEMPLATE")]
        key_template: Option<String>,

        /// 只做契约 / 分片 / 金额校验，不写库。
        #[arg(long)]
        dry_run: bool,
    },

    /// 自述：对象存储后端 + schema 版本 + 数据量（未来 HTTP `/healthz` 的同一份文档）。
    Healthz {
        /// local 后端根目录（env: WIST_COST_OBJECT_ROOT，默认 "."）。
        #[arg(long, env = "WIST_COST_OBJECT_ROOT")]
        objects_root: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("wist-cost: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let database_url = match cli.database_url.clone() {
        Some(url) => Some(url),
        None => std::env::var("DATABASE_URL").ok(),
    };

    match cli.command {
        Command::Migrate => {
            let store = connect(&database_url).await?;
            store.migrate().await.context("应用迁移失败")?;
            let schema = store.schema_status().await?;
            print_json(&json!({
                "command": "migrate",
                "ok": true,
                "schema": schema,
                "initialized": schema.initialized(),
            }));
            Ok(ExitCode::SUCCESS)
        }
        Command::Ingest {
            manifest,
            objects_root,
            key_template,
            dry_run,
        } => {
            ingest(
                &database_url,
                &manifest,
                objects_root.as_deref(),
                key_template.as_deref(),
                dry_run,
            )
            .await
        }
        Command::Healthz { objects_root } => healthz(&database_url, objects_root.as_deref()).await,
    }
}

async fn ingest(
    database_url: &Option<String>,
    manifest_path: &PathBuf,
    objects_root: Option<&std::path::Path>,
    key_template: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<ExitCode> {
    let raw = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("读不到 manifest {}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_str(&raw)
        .with_context(|| format!("manifest 不是契约 v1 的形状：{}", manifest_path.display()))?;

    let layout = match key_template {
        Some(template) => ShardKeyLayout::new(template).map_err(|err| anyhow::anyhow!("{err}"))?,
        None => ShardKeyLayout::design_default(),
    };
    let objects = build_object_store(objects_root)?;

    if dry_run {
        let batch = verify_batch(&manifest, objects.as_ref(), &layout).await?;
        print_json(&json!({
            "command": "ingest",
            "dry_run": true,
            "batch_id": manifest.batch_id,
            "range": format!(
                "{}/{}/{}/{}",
                manifest.range.tenant,
                manifest.range.source_code,
                manifest.range.billing_account_uin,
                manifest.range.period_label()
            ),
            "rows": batch.total_rows,
            "bytes": batch.total_bytes,
            "months": batch
                .months()
                .into_iter()
                .map(|(month, rows)| json!({"bill_month": month, "rows": rows}))
                .collect::<Vec<_>>(),
            "totals": batch.totals,
        }));
        return Ok(ExitCode::SUCCESS);
    }

    let store = connect(database_url).await?;
    let ingestor = Ingestor::new(&store, objects.as_ref(), layout);
    let report = ingestor
        .ingest(&manifest, &manifest_path.display().to_string())
        .await?;
    print_json(&json!({
        "command": "ingest",
        "dry_run": false,
        "report": report,
    }));
    Ok(ExitCode::SUCCESS)
}

async fn healthz(
    database_url: &Option<String>,
    objects_root: Option<&std::path::Path>,
) -> anyhow::Result<ExitCode> {
    let config = object_config(objects_root)?;
    let objects = config.describe();

    let (status, database, schema, exit) = match connect_optional(database_url).await {
        Err(err) => (
            "degraded",
            json!({"reachable": false, "error": err.to_string()}),
            serde_json::Value::Null,
            ExitCode::FAILURE,
        ),
        Ok(store) => match store.schema_status().await {
            Err(err) => (
                "degraded",
                json!({"reachable": true, "error": err.to_string()}),
                serde_json::Value::Null,
                ExitCode::FAILURE,
            ),
            // schema 没初始化就不要去数表：直接报"未迁移"，比一个 relation 不存在更像自述。
            Ok(schema) if !schema.initialized() => (
                "degraded",
                json!({"reachable": true, "initialized_schema": false}),
                json!(&schema),
                ExitCode::FAILURE,
            ),
            Ok(schema) => match store.stats().await {
                Ok(stats) => (
                    "ok",
                    json!({
                        "reachable": true,
                        "ranges": stats.ranges,
                        "published_versions": stats.published_versions,
                        "detail_rows": stats.detail_rows,
                    }),
                    json!({
                        "expected": schema.expected,
                        "version": schema.version,
                        "migrations": schema.migrations,
                        "initialized": true,
                    }),
                    ExitCode::SUCCESS,
                ),
                Err(err) => (
                    "degraded",
                    json!({"reachable": true, "error": err.to_string()}),
                    json!(&schema),
                    ExitCode::FAILURE,
                ),
            },
        },
    };

    print_json(&json!({
        "service": "wist-cost",
        "version": env!("CARGO_PKG_VERSION"),
        "rule_version": RULE_VERSION,
        "status": status,
        "objects": objects,
        "schema": schema,
        "database": database,
    }));
    Ok(exit)
}

fn object_config(objects_root: Option<&std::path::Path>) -> anyhow::Result<ObjectStoreConfig> {
    let mut config = ObjectStoreConfig::from_env(|name| std::env::var(name).ok())
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    if let (Some(root), ObjectStoreConfig::Local { .. }) = (objects_root, &config) {
        config = ObjectStoreConfig::Local {
            root: root.to_path_buf(),
        };
    }
    Ok(config)
}

fn build_object_store(
    objects_root: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn ObjectStore>> {
    object_config(objects_root)?
        .build()
        .map_err(|err| anyhow::anyhow!("{err}"))
}

async fn connect(database_url: &Option<String>) -> anyhow::Result<PgStore> {
    let url = database_url.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "缺少数据库连接串：给 --database-url，或设置 WIST_COST_DATABASE_URL / DATABASE_URL"
        )
    })?;
    PgStore::connect(url)
        .await
        .context("连接 PG 失败（凭据只从环境变量取，不落盘）")
}

async fn connect_optional(database_url: &Option<String>) -> anyhow::Result<PgStore> {
    match database_url {
        Some(url) => PgStore::connect(url).await.context("连接 PG 失败"),
        None => bail!("缺少数据库连接串（--database-url / WIST_COST_DATABASE_URL）"),
    }
}

fn print_json(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(err) => eprintln!("wist-cost: 输出 JSON 失败：{err}"),
    }
}
