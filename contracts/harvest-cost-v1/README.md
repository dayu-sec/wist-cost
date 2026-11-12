# harvest → cost 契约 v1

> 生产方：`wist-harvest`（只写分片与 manifest）；消费方：`wist-cost`（只认 manifest）。
> **契约与消费方同仓**：权威定义就是本目录，改动在这里改，两侧测试都要红一次。
> 设计依据（仓外文档）：`../../wist-cost-设计要点.md` §2、`../../wist-harvest-设计说明.md` §7。

## 1. 两个文件

| 文件 | 说明 |
| --- | --- |
| `manifest.schema.json` | 一个采集范围的清单：范围身份、分片列表、行数与校验和 |
| `detail.schema.json` | 分片里的一行：标准化字段 + 原文 |

分片是 NDJSON：一行一个 `detail` 对象，UTF-8，`\n` 分隔，**行序稳定**（`row_no` 从 1 起，与文件行顺序一致）。

## 2. 三条不可协商的规则

1. **分片不可变。** 补采 = 新 `batch_id` + 新分片。已发布分片的字节不允许变，`sha256` 是它的身份。
2. **manifest 是唯一发布凭据。** 消费方只认 `shards[].sha256` 与 `rows`；不接受"文件大小看起来对"这类判断。
3. **金额是十进制字符串。** `"10.000000000000"` 而不是 `10`。`null` 表示未知，`"0"` 表示确认为零，二者不可互换；退款为负。

## 3. 范围身份

`range` 的七元组唯一确定一个范围：

```
(tenant, source_code, vendor, billing_account_uin, cost_basis, period_start, period_end)
```

- `cost_basis = CONSUMPTION` → `period_start == period_end`，单日；
- `cost_basis = CHARGE` → 整月，`period_start` 为该月首日，`period_end` 为该月末日。

同一七元组重复发布 = **替换**，不是累加。

## 4. 版本规则

- 加字段：不升版本（消费方必须忽略未知字段）。
- 删字段 / 改语义 / 改枚举 / 改金额精度：升 `v2`，且两侧必须同时切。
- `schema` 取值固定为 `wist.harvest.cost.manifest.v1` / `wist.harvest.cost.detail.v1`。

## 5. 样例

- `fixtures/manifest-sample.json`
- `fixtures/detail-sample.ndjson`

两侧测试**直接读这两份**，不要各写一份自己的样例。
