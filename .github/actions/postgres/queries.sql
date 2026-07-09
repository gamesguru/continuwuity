/*
Created on Sat Apr 04 13:21:17 2026

@author: shane
*/

WITH baseline_commit AS (
    SELECT b.commit_hash
    FROM runs b
    WHERE {baseline_run_filter}
    ORDER BY b.run_date DESC LIMIT 1
),
baseline_runs AS (
    SELECT
        b2.id,
        b2.os,
        b2.arch,
        b2.profile,
        COALESCE(b2.room_version, '11') AS room_version,
        COALESCE(regexp_replace(btrim(b2.features, ' ,'), '[,\s]+', ' ', 'g'), '') AS features,
        b2.run_date,
        b2.n_pass,
        b2.n_fail,
        b2.n_skip
    FROM runs b2
    WHERE b2.commit_hash = (SELECT commit_hash FROM baseline_commit)
),
recent_runs AS (
    SELECT r.*
    FROM runs r
    WHERE r.n_pass > 0
      AND EXISTS (SELECT 1 FROM run_details rd WHERE rd.run_id = r.id)
    {like_filter}
    ORDER BY {order}
    LIMIT {limit}
),
run_regs AS (
    SELECT
        r.id,
        r.version_string,
        r.run_date,
        r.n_pass,
        r.n_fail,
        r.n_skip,
        r.profile,
        r.room_version,
        r.features,
        r.os,
        r.arch,
        counts.run_total,
        counts.detail_n_pass,
        counts.detail_n_skip,
        counts.detail_n_fail,
        counts.new_pass,
        counts.new_skip,
        counts.new_fail,
        counts.new_failures_list,
        counts.new_passes_list,
        mb_run_id.baseline_run_id,
        mb_run_id.baseline_n_pass,
        mb_run_id.baseline_n_fail,
        mb_run_id.baseline_n_skip
    FROM recent_runs r
    LEFT JOIN LATERAL (
        SELECT
            b2.id AS baseline_run_id,
            b2.n_pass AS baseline_n_pass,
            b2.n_fail AS baseline_n_fail,
            b2.n_skip AS baseline_n_skip
        FROM baseline_runs b2
        WHERE b2.os IS NOT DISTINCT FROM r.os
          AND b2.arch IS NOT DISTINCT FROM r.arch
          AND b2.profile IS NOT DISTINCT FROM r.profile
          AND b2.room_version IS NOT DISTINCT FROM COALESCE(r.room_version, '11')
          AND b2.features IS NOT DISTINCT FROM COALESCE(regexp_replace(btrim(r.features, ' ,'), '[,\s]+', ' ', 'g'), '')
        ORDER BY b2.run_date DESC, b2.id DESC
        LIMIT 1
    ) mb_run_id ON TRUE
    LEFT JOIN LATERAL (
        SELECT
            COUNT(*) as run_total,
            COUNT(*) FILTER (WHERE rd.status = 'pass') as detail_n_pass,
            COUNT(*) FILTER (WHERE rd.status = 'skip') as detail_n_skip,
            COUNT(*) FILTER (WHERE rd.status = 'fail') as detail_n_fail,
            COUNT(*) FILTER (WHERE rd.status = 'pass' AND (mb_run_id.baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'pass'))) as new_pass,
            COUNT(*) FILTER (WHERE rd.status = 'fail' AND (mb_run_id.baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'fail'))) as new_fail,
            COUNT(*) FILTER (WHERE rd.status = 'skip' AND (mb_run_id.baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'skip'))) as new_skip,
            STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name) FILTER (WHERE rd.status = 'fail' AND (mb_run_id.baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'fail'))) as new_failures_list,
            STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name) FILTER (WHERE rd.status = 'pass' AND (mb_run_id.baseline_run_id IS NOT NULL AND (mb.status IS NULL OR mb.status != 'pass'))) as new_passes_list
        FROM run_details rd
        LEFT JOIN run_details mb ON mb.test_name = rd.test_name AND mb.run_id = mb_run_id.baseline_run_id
        WHERE rd.run_id = r.id
    ) counts ON TRUE
    WHERE counts.run_total > 0
)
SELECT
    id AS run_id,
    version_string,
    to_char(run_date AT TIME ZONE '{tz_sql}', 'YYYY-MM-DD HH24:MI:SS') AS run_date,
    (n_pass + n_skip + n_fail) AS n_total,
    n_pass,
    n_skip,
    n_fail,
    new_pass,
    new_fail,
    {super_columns}
    profile,
    room_version,
    regexp_replace(btrim(features, ' ,'), '[,\s]+', ' ', 'g') AS features,
    os,
    arch,
    {columns_tail}
FROM
    run_regs
