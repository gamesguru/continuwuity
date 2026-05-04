/*
Created on Sat Apr 04 13:21:17 2026

@author: shane
*/

WITH run_regs AS (
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
        counts.new_pass,
        counts.new_skip,
        counts.new_fail,
        counts.new_failures_list,
        counts.new_passes_list
    FROM runs r
    LEFT JOIN LATERAL (
        SELECT
            COUNT(*) as run_total,
            COUNT(*) FILTER (WHERE rd.status = 'pass' AND mb.status IS NOT NULL AND mb.status != 'pass') as new_pass,
            COUNT(*) FILTER (WHERE rd.status = 'fail' AND mb.status IS NOT NULL AND mb.status != 'fail') as new_fail,
            COUNT(*) FILTER (WHERE rd.status = 'skip' AND mb.status IS NOT NULL AND mb.status != 'skip') as new_skip,
            STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name) FILTER (WHERE rd.status = 'fail' AND mb.status IS NOT NULL AND mb.status != 'fail') as new_failures_list,
            STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name) FILTER (WHERE rd.status = 'pass' AND mb.status IS NOT NULL AND mb.status != 'pass') as new_passes_list
        FROM run_details rd
        LEFT JOIN run_details mb ON mb.test_name = rd.test_name
            AND mb.run_id = (
                SELECT b2.id FROM runs b2
                WHERE b2.commit_hash = (
                    SELECT b.commit_hash FROM runs b
                    WHERE {baseline_run_filter}
                    ORDER BY b.run_date DESC LIMIT 1
                )
                  AND b2.os IS NOT DISTINCT FROM r.os
                  AND b2.arch IS NOT DISTINCT FROM r.arch
                  AND b2.profile IS NOT DISTINCT FROM r.profile
                  AND COALESCE(b2.room_version, '11') IS NOT DISTINCT FROM COALESCE(r.room_version, '11')
                ORDER BY b2.run_date DESC LIMIT 1
            )
        WHERE rd.run_id = r.id
    ) counts ON TRUE
    WHERE r.n_pass > 0 AND counts.run_total > 0
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
    profile,
    room_version,
    regexp_replace(btrim(features, ' ,'), '[,\s]+', ' ', 'g') AS features,
    os,
    arch,
    {columns_tail}
FROM
    run_regs
