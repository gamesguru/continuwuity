-- Incremental ever_passed UPSERT scoped to the current run.
-- Placeholders are replaced by sed at runtime:
--   {commit}, {arch}, {os}, {profile}, {room_version}, {features}
INSERT INTO ever_passed (test_name, rv, last_passed, last_commit, last_branch, branches)
SELECT
    rd.test_name,
    COALESCE(r.room_version, '11'),
    r.run_date::date::text,
    r.commit_hash,
    r.branch,
    ARRAY[r.branch]
FROM run_details rd
JOIN runs r ON rd.run_id = r.id
WHERE rd.status = 'pass'
  AND r.commit_hash = '{commit}'
  AND r.arch IS NOT DISTINCT FROM (NULLIF('{arch}', ''))
  AND r.os IS NOT DISTINCT FROM (NULLIF('{os}', ''))
  AND r.profile IS NOT DISTINCT FROM (NULLIF('{profile}', ''))
  AND r.room_version IS NOT DISTINCT FROM (NULLIF('{room_version}', ''))
  AND COALESCE(regexp_replace(btrim(r.features, ' ,'), '[,\s]+', ' ', 'g'), '') IS NOT DISTINCT FROM COALESCE(regexp_replace(btrim('{features}', ' ,'), '[,\s]+', ' ', 'g'), '')
ON CONFLICT (test_name, rv) DO UPDATE SET
    last_passed = GREATEST(ever_passed.last_passed, EXCLUDED.last_passed),
    last_commit = CASE
        WHEN EXCLUDED.last_passed > COALESCE(ever_passed.last_passed, '')
        THEN EXCLUDED.last_commit ELSE ever_passed.last_commit END,
    last_branch = CASE
        WHEN EXCLUDED.last_passed > COALESCE(ever_passed.last_passed, '')
        THEN EXCLUDED.last_branch ELSE ever_passed.last_branch END,
    branches = (
        SELECT ARRAY_AGG(DISTINCT b ORDER BY b)
        FROM UNNEST(ever_passed.branches || EXCLUDED.branches) AS b
        WHERE b IS NOT NULL
    );
