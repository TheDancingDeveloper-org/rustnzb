-- ── Groups ────────────────────────────────────────────────────────────────────
INSERT INTO groups (id, name, description, subscribed, article_count, first_article, last_article, last_scanned)
VALUES
  (1, 'alt.test',          'General testing',  1, 100, 1, 100,  50),
  (2, 'alt.binaries.test', 'Binary testing',   1, 500, 1, 500, 500),
  (3, 'misc.test',         'Misc group',       0,  30, 1,  30,   0);

-- ── Headers for group 1 ────────────────────────────────────────────────────────
INSERT INTO headers (id, group_id, article_num, subject, author, date, message_id, references_, bytes, lines)
VALUES
  (1, 1, 1, 'Test Post Alpha',    'alice@test.com', '2026-03-01 10:00:00', 'msg1@test', '',         512,     10),
  (2, 1, 2, 'Re: Test Post Alpha','bob@test.com',   '2026-03-01 11:00:00', 'msg2@test', 'msg1@test',256,      5),
  (3, 1, 3, 'Binary File [1/3]',  'poster@news',    '2026-03-02 09:00:00', 'bin1@test', '',       2048000, 5000),
  (4, 1, 4, 'Binary File [2/3]',  'poster@news',    '2026-03-02 09:01:00', 'bin2@test', '',       2048000, 5000),
  (5, 1, 5, 'Binary File [3/3]',  'poster@news',    '2026-03-02 09:02:00', 'bin3@test', '',       2048000, 5000);

-- ── Queue (paused jobs for queue UI tests) ────────────────────────────────────
-- Keep the deterministic queue from being consumed before Playwright reaches it.
INSERT INTO settings (key, value) VALUES ('globally_paused', 'true');

INSERT INTO queue (id, name, category, status, priority, total_bytes, downloaded_bytes,
                   file_count, article_count, added_at, work_dir, output_dir)
VALUES
  ('queue-job-1', 'Test.Movie.2025.mkv', 'movies', 'paused', 1,
   1073741824, 536870912, 3, 300,
   '2026-03-01T10:00:00Z',
   'e2e/test-data/incomplete/queue-job-1',
   'e2e/test-data/complete/movies/Test.Movie.2025.mkv'),
  ('queue-job-2', 'Another.Show.S01E01', 'tv', 'queued', 2,
   524288000, 0, 2, 150,
   '2026-03-01T11:00:00Z',
   'e2e/test-data/incomplete/queue-job-2',
   'e2e/test-data/complete/tv/Another.Show.S01E01');

-- ── History entries ────────────────────────────────────────────────────────────
INSERT INTO history (id, name, category, status, total_bytes, downloaded_bytes,
                     added_at, completed_at, output_dir, stages, error_message,
                     server_stats, job_logs)
VALUES
  ('hist-completed-1', 'Completed.Movie.2025.mkv', 'movies', 'completed',
   2147483648, 2147483648,
   '2026-03-01T08:00:00Z', '2026-03-01T09:30:00Z',
   'e2e/test-data/complete/movies/Completed.Movie.2025.mkv',
   '[{"name":"Download","status":"completed","detail":"2.0 GB in 90m"},{"name":"Verify","status":"completed","detail":"All OK"},{"name":"Extract","status":"completed","detail":"Extracted 1 file"}]',
   NULL, '[{"server_id":"seed-server","server_name":"Seed News","articles_downloaded":1998,"articles_failed":2,"bytes_downloaded":2147483648}]', '[]'),

  ('hist-failed-1', 'Failed.Show.S02E05.mkv', 'tv', 'failed',
   1073741824, 524288000,
   '2026-03-02T10:00:00Z', '2026-03-02T10:45:00Z',
   'e2e/test-data/complete/tv/Failed.Show.S02E05.mkv',
   '[{"name":"Download","status":"completed","detail":"500 MB"},{"name":"Verify","status":"failed","detail":"Par2 repair failed"}]',
   'Article not found on server: test-msg@test.com',
   '[{"server_id":"seed-server","server_name":"Seed News","articles_downloaded":450,"articles_failed":50,"bytes_downloaded":524288000}]', '[]'),

  ('hist-completed-2', 'Good.Podcast.EP100.mp3', 'Default', 'completed',
   52428800, 52428800,
   '2026-03-03T07:00:00Z', '2026-03-03T07:05:00Z',
   'e2e/test-data/complete/Default/Good.Podcast.EP100.mp3',
   '[{"name":"Download","status":"completed","detail":"50 MB"}]',
   NULL, '[{"server_id":"seed-server","server_name":"Seed News","articles_downloaded":100,"articles_failed":0,"bytes_downloaded":52428800}]', '[]');

INSERT INTO download_statistics (job_id, completed_at, status, total_bytes,
                                 downloaded_bytes, duration_secs, average_speed_bps,
                                 server_stats)
SELECT id, completed_at, status, total_bytes, downloaded_bytes,
       MAX(0, (julianday(completed_at) - julianday(added_at)) * 86400.0),
       CASE WHEN julianday(completed_at) > julianday(added_at)
            THEN CAST(downloaded_bytes / ((julianday(completed_at) - julianday(added_at)) * 86400.0) AS INTEGER)
            ELSE 0 END,
       server_stats
FROM history;

-- ── RSS items ─────────────────────────────────────────────────────────────────
INSERT INTO rss_items (id, feed_name, title, url, published_at, first_seen_at,
                        downloaded, category, size_bytes)
VALUES
  ('rss-item-1', 'Test Feed', 'New Release S01E01 720p',
   'https://example.com/nzb/12345', '2026-03-04T10:00:00Z', '2026-03-04T10:01:00Z',
   0, 'tv', 1073741824),
  ('rss-item-2', 'Test Feed', 'New Release S01E02 720p',
   'https://example.com/nzb/12346', '2026-03-04T11:00:00Z', '2026-03-04T11:01:00Z',
   0, 'tv', 1073741824),
  ('rss-item-3', 'Test Feed', 'Already Downloaded Movie',
   'https://example.com/nzb/12347', '2026-03-03T10:00:00Z', '2026-03-03T10:01:00Z',
   1, 'movies', 2147483648);

-- ── RSS rules ─────────────────────────────────────────────────────────────────
INSERT INTO rss_rules (id, name, feed_name, category, priority, match_regex, enabled)
VALUES
  ('rss-rule-1', 'New Release auto-grab', 'Test Feed', 'tv', 1,
   'S\d{2}E\d{2}', 1),
  ('rss-rule-2', 'Disabled rule', 'Test Feed', 'movies', 1,
   '4K.*UHD', 0);
