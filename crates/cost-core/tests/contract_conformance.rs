//! 契约自检：`contracts/harvest-cost-v1/` 的样例必须**不经 PG** 就能验通。
//!
//! 为什么单独一个用例：原来只有 `crates/cost-store/tests/ingest_pg.rs` 读这两份样例，
//! 而它是 `#[ignore]`（要一次性 PG）。也就是说**没起库的时候契约漂移无人看守** ——
//! schema 改了、样例没跟上、字段语义变了，`cargo test` 依然全绿。
//!
//! 这里把契约当成被测对象：样例 → 解析 → `validate` → `verify_shard`，全在纯逻辑层跑完。

use std::path::{Path, PathBuf};

use cost_core::{DETAIL_SCHEMA, MANIFEST_SCHEMA, Manifest, verify_shard};

fn contract_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/harvest-cost-v1")
        .canonicalize()
        .expect("找不到 contracts/harvest-cost-v1（契约应与本仓同仓）")
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = contract_dir().join("fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("读 {}: {err}", path.display()))
}

fn fixture_manifest() -> Manifest {
    let raw = fixture_bytes("manifest-sample.json");
    serde_json::from_slice(&raw).expect("契约 manifest 样例形状必须与 `Manifest` 一致")
}

#[test]
fn the_contract_fixtures_are_present_and_parse() {
    let manifest = fixture_manifest();
    assert_eq!(manifest.schema, MANIFEST_SCHEMA);
    manifest.validate().expect("契约 manifest 样例必须自洽");
    assert!(!manifest.shards.is_empty(), "样例至少要有一个分片");

    let shard = &manifest.shards[0];
    let bytes = fixture_bytes("detail-sample.ndjson");
    let verified = verify_shard(&manifest, shard, &bytes).expect("契约分片样例必须验通");
    assert_eq!(
        verified.details.len() as u64,
        shard.rows,
        "manifest 声明 {} 行，实际 {} 行",
        shard.rows,
        verified.details.len()
    );
    assert_eq!(manifest.total_rows, shard.rows, "单分片 batch 的总行数");
    assert_eq!(
        verified.details[0].line.schema, DETAIL_SCHEMA,
        "分片行的 schema 必须与常量一致"
    );
    assert_eq!(verified.details[0].row_no, 1, "行号从 1 起且与文件行序一致");
}

#[test]
fn a_tampered_shard_is_rejected() {
    // 契约 §2 规则 1：已发布分片的字节不允许变，`sha256` 是它的身份。
    let manifest = fixture_manifest();
    let shard = &manifest.shards[0];
    let mut bytes = fixture_bytes("detail-sample.ndjson");
    assert!(!bytes.is_empty());
    // 改一个字节（保持合法 UTF-8）：sha256 必须变。
    bytes[0] = if bytes[0] == b'{' { b'[' } else { b'{' };

    let err = verify_shard(&manifest, shard, &bytes).expect_err("字节变了就必须拒收");
    assert!(
        format!("{err}").contains("sha256"),
        "拒收理由要指向 sha256，实际：{err}"
    );
}

#[test]
fn an_unknown_field_does_not_break_parsing() {
    // 契约 §4：加字段不升版本，消费方必须忽略未知字段。
    let mut raw: serde_json::Value =
        serde_json::from_slice(&fixture_bytes("manifest-sample.json")).expect("json");
    raw["gateway_metadata"] = serde_json::json!({ "collector": "future-version" });
    let manifest: Manifest = serde_json::from_value(raw).expect("未知字段不能让解析失败");
    manifest.validate().expect("加字段后仍应自洽");
}

/// 改这两个字符串就是改契约，两侧必须同时切。
#[test]
fn the_schema_constants_match_the_contract() {
    assert_eq!(MANIFEST_SCHEMA, "wist.harvest.cost.manifest.v1");
    assert_eq!(DETAIL_SCHEMA, "wist.harvest.cost.detail.v1");
}
