-- 分区管理：建月分区 + 18 个月保留期清理。
--
-- 设计 §4.1：`cost_detail` 按 `bill_month` 分区，保留 18 个月（与云 API 可回补窗口对齐）。
-- PG 没有 `DROP PARTITION` DDL，等价动作是对分区表执行 `drop table`（顺带 detach）。

-- 分区表名：cost_detail_YYYY_MM
create or replace function cost_month_partition_name(p_month text)
returns text
language sql
immutable
as $$
  select 'cost_detail_' || replace(p_month, '-', '_');
$$;

-- 幂等建分区。入账写 staging 前调用，所以"忘记提前建分区"不会变成运行时失败。
create or replace function cost_ensure_month_partition(p_month text)
returns text
language plpgsql
as $$
declare
  v_name  text;
  v_start text;
  v_end   text;
begin
  if p_month !~ '^[0-9]{4}-(0[1-9]|1[0-2])$' then
    raise exception 'bill_month 必须是 YYYY-MM，实际 %', p_month;
  end if;

  v_name  := cost_month_partition_name(p_month);
  v_start := p_month;
  v_end   := to_char((to_date(p_month || '-01', 'YYYY-MM-DD') + interval '1 month')::date, 'YYYY-MM');

  if to_regclass(v_name) is null then
    execute format(
      'create table if not exists %I partition of cost_detail for values from (%L) to (%L)',
      v_name, v_start, v_end
    );
  end if;

  return v_name;
end;
$$;

-- 保留最近 p_keep_months 个自然月（含当月），更早的分区整块 drop。
-- 返回被删掉的分区名，便于入账/运维日志留痕。
create or replace function cost_prune_partitions(p_keep_months integer default 18)
returns setof text
language plpgsql
as $$
declare
  v_cutoff text;
  v_rec    record;
  v_month  text;
begin
  if p_keep_months < 1 then
    raise exception 'p_keep_months 必须 >= 1，实际 %', p_keep_months;
  end if;

  v_cutoff := to_char(
    date_trunc('month', now()) - make_interval(months => p_keep_months - 1),
    'YYYY-MM'
  );

  for v_rec in
    select child.relname as name
    from pg_inherits i
    join pg_class child on child.oid = i.inhrelid
    join pg_class parent on parent.oid = i.inhparent
    where parent.relname = 'cost_detail'
      and child.relname ~ '^cost_detail_[0-9]{4}_[0-9]{2}$'
  loop
    v_month := replace(substring(v_rec.name from '^cost_detail_([0-9]{4}_[0-9]{2})$'), '_', '-');
    if v_month < v_cutoff then
      execute format('drop table if exists %I', v_rec.name);
      return next v_rec.name;
    end if;
  end loop;

  return;
end;
$$;

comment on function cost_prune_partitions(integer) is
  '成本明细保留期：保留最近 N 个自然月（默认 18）。整分区 drop，不做逐行 delete。';
