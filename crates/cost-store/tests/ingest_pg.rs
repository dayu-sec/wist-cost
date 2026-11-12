//! 入账契约测试（需要 PG）：**直接读** `contracts/harvest-cost-v1/fixtures/` 的两份样例。
//!
//! 这组用例标了 `#[ignore]`：默认 `cargo test` 会显示 `ignored`，不会偷偷"绿"。
//! 跑法：
//!
//! ```sh
//! WIST_COST_TEST_DATABASE_URL=postgres://postgres:adminpw@127.0.0.1:55999/wist_cost_test \
//!   cargo test -p cost-store --test ingest_pg -- --include-ignored
//! ```
//!
//! 没有 `WIST_COST_TEST_DATABASE_URL` 时直接 panic（宁可红，也不静默跳过）。
//! 注意：测试会 `truncate` 目标库的 `cost_range*` / `cost_detail`，只指向临时库/一次性容器。

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use cost_core::objects::ShardKeyLayout;
use cost_core::{Manifest, Money};
use cost_store::{IngestReport, Ingestor, LocalObjectStore, PgStore, PublishOutcome, StoreError};

/// 串行化：所有用例共用一个库，靠锁 + truncate 隔离。
static DB_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

struct Harness {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    store: PgStore,
    objects_root: PathBuf,
    _dir: tempfile::TempDir,
}

impl Harness {
    /// 连测试库、应用迁移、清空数据。
    async fn new() -> Self {
        let url = std::env::var("WIST_COST_TEST_DATABASE_URL").unwrap_or_else(|_| {
            panic!(
                "这组用例需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL，\
                 并加 --include-ignored 运行"
            )
        });
        let guard = DB_LOCK.lock().await;

        let store = PgStore::connect(&url)
            .await
            .unwrap_or_else(|err| panic!("连接测试库失败（{url}）：{err}"));
        store.migrate().await.expect("迁移测试库失败");
        sqlx::query("truncate cost_range, cost_range_version, cost_detail cascade")
            .execute(store.pool())
            .await
            .expect("清空测试库失败");

        let dir = tempfile::tempdir().expect("建临时对象目录");
        Self {
            _guard: guard,
            store,
            objects_root: dir.path().to_path_buf(),
            _dir: dir,
        }
    }

    async fn ingest(&self, manifest: &Manifest) -> Result<IngestReport, StoreError> {
        let objects = LocalObjectStore::new(&self.objects_root);
        let ingestor = Ingestor::new(&self.store, &objects, ShardKeyLayout::design_default());
        ingestor
            .ingest(manifest, "fixtures/manifest-sample.json")
            .await
    }

    async fn scalar_i64(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(self.store.pool())
            .await
            .unwrap_or_else(|err| panic!("查询失败（{sql}）：{err}"))
    }

    async fn scalar_text(&self, sql: &str) -> Option<String> {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(self.store.pool())
            .await
            .unwrap_or_else(|err| panic!("查询失败（{sql}）：{err}"))
    }
}

/// 契约样例目录（可用 `WIST_COST_CONTRACT_FIXTURES` 覆盖）。
fn fixtures_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("WIST_COST_CONTRACT_FIXTURES") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../contracts/harvest-cost-v1/fixtures")
        .canonicalize()
        .expect("找不到 contracts/harvest-cost-v1/fixtures")
}

/// 直接读契约样例 manifest（不做任何自造样例）。
fn fixture_manifest_value() -> serde_json::Value {
    let path = fixtures_dir().join("manifest-sample.json");
    let raw = std::fs::read_to_string(&path).expect("读契约 manifest 样例");
    serde_json::from_str(&raw).expect("契约 manifest 样例是合法 JSON")
}

/// 直接读契约样例分片。
fn fixture_shard_bytes() -> Vec<u8> {
    std::fs::read(fixtures_dir().join("detail-sample.ndjson")).expect("读契约分片样例")
}

fn manifest_from(value: serde_json::Value) -> Manifest {
    serde_json::from_value(value).expect("manifest 形状")
}

/// 把分片字节放到设计 §4 的对象键上（键由 manifest 推导，和入账侧完全同一套布局）。
fn write_shard(root: &Path, manifest: &Manifest, shard_id: &str, bytes: &[u8]) {
    let key = ShardKeyLayout::design_default().key_for(manifest, shard_id);
    let path = root.join(key);
    std::fs::create_dir_all(path.parent().expect("对象键有父目录")).expect("建对象目录");
    std::fs::write(path, bytes).expect("写分片");
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

// ── 1. 契约样例完成一次入账 ─────────────────────────────────────────────────
#[tokio::test]
#[ignore = "需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL 后加 --include-ignored"]
async fn fixture_manifest_publishes_an_effective_version() {
    let harness = Harness::new().await;

    let manifest = manifest_from(fixture_manifest_value());
    let bytes = fixture_shard_bytes();
    write_shard(&harness.objects_root, &manifest, "0001", &bytes);

    let report = harness.ingest(&manifest).await.expect("样例入账应成功");
    assert_eq!(report.outcome, PublishOutcome::Published);
    assert_eq!(report.rows, 3);
    assert_eq!(report.bytes, bytes.len() as u64);
    assert_eq!(report.superseded_version_id, None);
    assert_eq!(
        report
            .months
            .iter()
            .map(|m| m.bill_month.as_str())
            .collect::<Vec<_>>(),
        vec!["2026-11"]
    );

    // 明细落在按 bill_month 建的分区里。
    assert_eq!(
        harness
            .scalar_text("select to_regclass('cost_detail_2026_11')::text")
            .await
            .as_deref(),
        Some("cost_detail_2026_11")
    );

    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_range_version where state = 'published'")
            .await,
        1
    );
    assert_eq!(
        harness.scalar_i64("select count(*) from cost_detail").await,
        3
    );
    assert_eq!(
        harness
            .scalar_text("select sum(real_cost)::text from cost_detail")
            .await
            .as_deref(),
        Some("9.300000000000")
    );
    assert_eq!(
        harness
            .scalar_text("select rule_version from cost_range_version limit 1")
            .await
            .as_deref(),
        Some(cost_core::RULE_VERSION)
    );
    assert_eq!(
        harness
            .scalar_text("select period_source from cost_range_version limit 1")
            .await
            .as_deref(),
        Some("cloud_api")
    );

    // shard_id + row_no 指向原文，行号从 1 起、与文件行序一致。
    assert_eq!(
        harness
            .scalar_i64(
                "select count(*) from cost_detail
                 where shard_id = '0001' and row_no in (1, 2, 3)",
            )
            .await,
        3
    );
}

// ── 2. 金额语义：null / "0" / 负退款 ────────────────────────────────────────
#[tokio::test]
#[ignore = "需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL 后加 --include-ignored"]
async fn amounts_keep_null_zero_and_negative_distinct() {
    let harness = Harness::new().await;

    let manifest = manifest_from(fixture_manifest_value());
    write_shard(
        &harness.objects_root,
        &manifest,
        "0001",
        &fixture_shard_bytes(),
    );
    harness.ingest(&manifest).await.expect("样例入账应成功");

    // 未知金额：null，不是 0。
    assert_eq!(
        harness
            .scalar_text(
                "select (cash_amount is null)::text from cost_detail
                 where resource_key = 'p_cbs:UNALLOCATED'",
            )
            .await
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        harness
            .scalar_text(
                "select count(*)::text from cost_detail
                 where resource_key = 'p_cbs:UNALLOCATED' and cash_amount is null",
            )
            .await
            .as_deref(),
        Some("1")
    );

    // 确认为零：数值是 0，且不是 null。
    assert_eq!(
        harness
            .scalar_text(
                "select (incentive_amount is not null and incentive_amount = 0)::text
                 from cost_detail where resource_key = 'p_cvm:ins-example01'",
            )
            .await
            .as_deref(),
        Some("true")
    );

    // 退款保留负号。
    assert_eq!(
        harness
            .scalar_text(
                "select real_cost::text from cost_detail where resource_key = 'p_eip:eip-example01'"
            )
            .await
            .as_deref(),
        Some("-3.200000000000")
    );
    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_detail where real_cost < 0")
            .await,
        1
    );
}

// ── 3. 同一 manifest 连入账两次：行数不变、金额不翻倍、有效版本只有一个 ──────
#[tokio::test]
#[ignore = "需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL 后加 --include-ignored"]
async fn replaying_the_same_manifest_changes_nothing() {
    let harness = Harness::new().await;

    let manifest = manifest_from(fixture_manifest_value());
    write_shard(
        &harness.objects_root,
        &manifest,
        "0001",
        &fixture_shard_bytes(),
    );

    let first = harness.ingest(&manifest).await.expect("首次入账");
    let second = harness.ingest(&manifest).await.expect("重放入账");
    assert_eq!(first.outcome, PublishOutcome::Published);
    assert_eq!(second.outcome, PublishOutcome::Unchanged);
    assert_eq!(second.version_id, first.version_id, "重放不新建版本");

    assert_eq!(
        harness.scalar_i64("select count(*) from cost_detail").await,
        3
    );
    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_range_version where state = 'published'")
            .await,
        1
    );
    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_range_version")
            .await,
        1,
        "重放连 staging 版本都不该留"
    );
    assert_eq!(
        harness
            .scalar_text("select sum(real_cost)::text from cost_detail")
            .await
            .as_deref(),
        Some("9.300000000000"),
        "金额不能翻倍"
    );
}

// ── 4. 同范围新批次：整段替换，旧版本保留可回滚 ─────────────────────────────
#[tokio::test]
#[ignore = "需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL 后加 --include-ignored"]
async fn new_batch_for_same_range_supersedes_the_old_version() {
    let harness = Harness::new().await;

    let bytes = fixture_shard_bytes();
    let mut raw = fixture_manifest_value();
    let manifest = manifest_from(raw.clone());
    write_shard(&harness.objects_root, &manifest, "0001", &bytes);
    let first = harness.ingest(&manifest).await.expect("首次入账");

    // 同一七元组、新 batch_id（分片字节不变，补采的常见形态）。
    raw["batch_id"] = serde_json::json!("tencent-cloud-prod:cost-20261113T003000Z");
    let replayed = manifest_from(raw);
    write_shard(&harness.objects_root, &replayed, "0001", &bytes);
    let second = harness.ingest(&replayed).await.expect("补采入账");

    assert_eq!(second.outcome, PublishOutcome::Published);
    assert_eq!(second.superseded_version_id, Some(first.version_id));

    // 有效版本只有一个，且只有它的行参与汇总。
    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_range_version where state = 'published'")
            .await,
        1
    );
    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_range where tenant = 'tenant-example'")
            .await,
        1,
        "七元组相同 → 还是同一个范围"
    );
    assert_eq!(
        harness
            .scalar_i64(&format!(
                "select count(*) from cost_detail where version_id = {}",
                second.version_id
            ))
            .await,
        3
    );
    assert_eq!(
        harness
            .scalar_text(&format!(
                "select sum(real_cost)::text from cost_detail where version_id = {}",
                second.version_id
            ))
            .await
            .as_deref(),
        Some("9.300000000000"),
        "替换不是累加：有效版本金额不能翻倍"
    );

    // 旧版本还在（可回滚），并被标成 superseded。
    assert_eq!(
        harness
            .scalar_text(&format!(
                "select state || ':' || coalesce(superseded_by::text, '-')
                 from cost_range_version where id = {}",
                first.version_id
            ))
            .await
            .as_deref(),
        Some(format!("superseded:{}", second.version_id).as_str())
    );
    assert_eq!(
        harness.scalar_i64("select count(*) from cost_detail").await,
        6,
        "两个版本各自的 3 行都保留（可回滚），但只有有效版本参与汇总"
    );
}

// ── 5. sha256 与 manifest 不符 → 整批拒绝，不产生有效版本 ───────────────────
#[tokio::test]
#[ignore = "需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL 后加 --include-ignored"]
async fn sha256_mismatch_rejects_the_batch() {
    let harness = Harness::new().await;

    let manifest = manifest_from(fixture_manifest_value());
    let mut tampered = fixture_shard_bytes();
    tampered.push(b'\n'); // 字节变了，sha256 一定不符
    write_shard(&harness.objects_root, &manifest, "0001", &tampered);

    let err = harness.ingest(&manifest).await.expect_err("必须拒绝");
    assert_eq!(err.code(), "sha256_mismatch");

    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_range_version")
            .await,
        0,
        "拒绝的批次不留版本行"
    );
    assert_eq!(
        harness.scalar_i64("select count(*) from cost_detail").await,
        0,
        "拒绝的批次不留明细"
    );
    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_range_version where state = 'published'")
            .await,
        0
    );
}

// ── 6. 行数与 manifest 不符 → 整批拒绝 ──────────────────────────────────────
#[tokio::test]
#[ignore = "需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL 后加 --include-ignored"]
async fn row_count_mismatch_rejects_the_batch() {
    let harness = Harness::new().await;

    let full = fixture_shard_bytes();
    let text = String::from_utf8(full.clone()).expect("样例是 UTF-8");
    let truncated = format!("{}\n", text.lines().take(2).collect::<Vec<_>>().join("\n"));

    // sha256 与截断后的字节一致（否则先被 sha 拦住，测不到行数分支）。
    let mut raw = fixture_manifest_value();
    raw["shards"][0]["sha256"] = serde_json::json!(sha256_hex(truncated.as_bytes()));
    raw["shards"][0]["bytes"] = serde_json::json!(truncated.len());
    let manifest = manifest_from(raw);
    write_shard(
        &harness.objects_root,
        &manifest,
        "0001",
        truncated.as_bytes(),
    );

    let err = harness.ingest(&manifest).await.expect_err("必须拒绝");
    assert_eq!(err.code(), "row_count_mismatch");
    assert_eq!(
        harness.scalar_i64("select count(*) from cost_detail").await,
        0
    );
    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_range_version")
            .await,
        0
    );
}

// ── 7. 七元组任一字段不同 → 不同范围，互不覆盖 ─────────────────────────────
#[tokio::test]
#[ignore = "需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL 后加 --include-ignored"]
async fn a_different_seven_tuple_is_a_different_range() {
    let harness = Harness::new().await;

    let bytes = fixture_shard_bytes();
    let consumption = manifest_from(fixture_manifest_value());
    write_shard(&harness.objects_root, &consumption, "0001", &bytes);
    let consumption_report = harness
        .ingest(&consumption)
        .await
        .expect("CONSUMPTION 入账");

    // 同租户同账号，只把口径换成整月 CHARGE（cost_date 仍在 11 月内）。
    let mut raw = fixture_manifest_value();
    raw["batch_id"] = serde_json::json!("tencent-cloud-prod:charge-202611");
    raw["range"]["cost_basis"] = serde_json::json!("CHARGE");
    raw["range"]["period_start"] = serde_json::json!("2026-11-01");
    raw["range"]["period_end"] = serde_json::json!("2026-11-30");
    let charge = manifest_from(raw.clone());
    write_shard(&harness.objects_root, &charge, "0001", &bytes);
    let charge_report = harness.ingest(&charge).await.expect("CHARGE 入账");

    assert_ne!(consumption_report.range_id, charge_report.range_id);
    assert_eq!(
        charge_report.superseded_version_id, None,
        "不同范围互不覆盖"
    );

    assert_eq!(
        harness.scalar_i64("select count(*) from cost_range").await,
        2
    );
    assert_eq!(
        harness
            .scalar_i64("select count(*) from cost_range_version where state = 'published'")
            .await,
        2
    );
    assert_eq!(
        harness.scalar_i64("select count(*) from cost_detail").await,
        6
    );
    assert_eq!(
        harness
            .scalar_text("select sum(real_cost)::text from cost_detail")
            .await
            .as_deref(),
        Some("18.600000000000"),
        "两个范围各 9.3，互不覆盖也不互相抹掉"
    );

    // 再改一个字段（账单账号）同样只新增范围。
    raw["batch_id"] = serde_json::json!("tencent-cloud-prod:charge-202611-other-uin");
    raw["range"]["billing_account_uin"] = serde_json::json!("100000000002");
    let other_uin = manifest_from(raw);
    write_shard(&harness.objects_root, &other_uin, "0001", &bytes);
    harness
        .ingest(&other_uin)
        .await
        .expect("另一个账单账号入账");
    assert_eq!(
        harness.scalar_i64("select count(*) from cost_range").await,
        3
    );
}

// ── 8. 迁移与自述：schema 版本、分区函数 ────────────────────────────────────
#[tokio::test]
#[ignore = "需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL 后加 --include-ignored"]
async fn schema_is_self_describing_and_partitions_are_idempotent() {
    let harness = Harness::new().await;

    let status = harness.store.schema_status().await.expect("读 schema 版本");
    assert_eq!(status.version.as_deref(), Some(cost_store::SCHEMA_VERSION));
    assert!(status.initialized());
    assert!(status.migrations >= 2, "两个迁移都应被应用");

    // 建分区幂等。
    let first: String = sqlx::query_scalar("select cost_ensure_month_partition('2026-11')")
        .fetch_one(harness.store.pool())
        .await
        .expect("建分区");
    let again: String = sqlx::query_scalar("select cost_ensure_month_partition('2026-11')")
        .fetch_one(harness.store.pool())
        .await
        .expect("重复建分区");
    assert_eq!(first, "cost_detail_2026_11");
    assert_eq!(first, again);

    // 保留期函数可用（没有更早分区时什么都不删）。
    let dropped: Vec<String> = sqlx::query_scalar("select * from cost_prune_partitions(18)")
        .fetch_all(harness.store.pool())
        .await
        .expect("清理分区");
    assert!(dropped.is_empty(), "没有超期分区：{dropped:?}");

    // 空范围也能发布：0 行、0 明细，仍然是有效版本。
    let mut raw = fixture_manifest_value();
    raw["batch_id"] = serde_json::json!("tencent-cloud-prod:cost-empty");
    raw["shards"] = serde_json::json!([]);
    raw["total_rows"] = serde_json::json!(0);
    let empty = manifest_from(raw);
    let report = harness.ingest(&empty).await.expect("空范围入账");
    assert_eq!(report.outcome, PublishOutcome::Published);
    assert_eq!(report.rows, 0);
    assert_eq!(
        harness.scalar_i64("select count(*) from cost_detail").await,
        0
    );
}

// ── 9. 金额列口径：库里是 numeric，不是浮点 ────────────────────────────────
#[tokio::test]
#[ignore = "需要一次性 PG：设置 WIST_COST_TEST_DATABASE_URL 后加 --include-ignored"]
async fn money_columns_are_numeric_with_twelve_decimals() {
    let harness = Harness::new().await;

    assert_eq!(
        harness
            .scalar_text(
                "select data_type || '(' || numeric_precision || ',' || numeric_scale || ')'
                 from information_schema.columns
                 where table_name = 'cost_detail' and column_name = 'real_cost'",
            )
            .await
            .as_deref(),
        Some("numeric(30,12)"),
        "金额列必须是 numeric(30,12)，不能是 float/double"
    );

    let manifest = manifest_from(fixture_manifest_value());
    write_shard(
        &harness.objects_root,
        &manifest,
        "0001",
        &fixture_shard_bytes(),
    );
    harness.ingest(&manifest).await.expect("样例入账");

    // 12 位小数在库里逐位保留。
    assert_eq!(
        harness
            .scalar_text("select voucher_amount::text from cost_detail where resource_key = 'p_cvm:ins-example01'")
            .await
            .as_deref(),
        Some("1.000000000000")
    );
    assert_eq!(
        Money::parse("9.300000000000").unwrap(),
        Money::parse("9.3").unwrap()
    );
}
