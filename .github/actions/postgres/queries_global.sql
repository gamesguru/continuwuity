/*
Global "ever-passed" regression query.
Uses mv_ever_passed materialized view instead of a single baseline commit.
A test is a regression if it fails now but has ever passed in any prior run.
*/

WITH recent_runs AS (
    SELECT r.*
    FROM runs r
    WHERE r.n_pass > 0
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
        counts.new_pass,
        counts.new_skip,
        counts.new_fail,
        counts.new_failures_list,
        counts.new_passes_list,
        counts.date_last_passed,
        counts.branches_passed_on,
        counts.last_commit_passed
    FROM recent_runs r
    LEFT JOIN LATERAL (
        SELECT
            COUNT(*) as run_total,
            COUNT(*) FILTER (WHERE rd.status = 'pass' AND ep.test_name IS NULL) as new_pass,
            COUNT(*) FILTER (WHERE rd.status = 'fail' AND ep.test_name IS NOT NULL) as new_fail,
            COUNT(*) FILTER (WHERE rd.status = 'skip') as new_skip,
            STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name)
                FILTER (WHERE rd.status = 'fail' AND ep.test_name IS NOT NULL) as new_failures_list,
            STRING_AGG(rd.test_name, E'\n' ORDER BY rd.test_name)
                FILTER (WHERE rd.status = 'pass' AND ep.test_name IS NULL) as new_passes_list,
            STRING_AGG(COALESCE(ep.last_passed, 'never'), E'\n' ORDER BY rd.test_name)
                FILTER (WHERE rd.status = 'fail' AND ep.test_name IS NOT NULL) as date_last_passed,
            STRING_AGG(COALESCE(ep.branches, '[]'), E'\n' ORDER BY rd.test_name)
                FILTER (WHERE rd.status = 'fail' AND ep.test_name IS NOT NULL) as branches_passed_on,
            STRING_AGG(COALESCE(ep.last_commit, ''), E'\n' ORDER BY rd.test_name)
                FILTER (WHERE rd.status = 'fail' AND ep.test_name IS NOT NULL) as last_commit_passed
        FROM run_details rd
        LEFT JOIN mv_ever_passed ep
            ON ep.test_name = rd.test_name
            AND ep.rv IS NOT DISTINCT FROM COALESCE(r.room_version, '11')
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
    profile,
    room_version,
    regexp_replace(btrim(features, ' ,'), '[,\s]+', ' ', 'g') AS features,
    os,
    arch,
    {columns_tail}
FROM
    run_regs
