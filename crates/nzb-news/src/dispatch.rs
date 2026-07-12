//! Priority-aware server selection and cascade-reset logic.
//!
//! Given an [`Article`] and a set of [`Server`]s, pick the next server to
//! dispatch the fetch against. The rules (applied in order):
//!
//! 1. Skip disabled, penalised, or fully-failed servers.
//! 2. Skip any server already in the article's try-list, the article's
//!    parent file's try-list, or the article's parent job's try-list.
//! 3. Among the remaining candidates, pick the **highest priority** that is
//!    usable *right now* (has an idle wrapper available, outside any ramp-up
//!    window). If the best candidate is a higher-priority server that is
//!    temporarily unavailable (e.g. ramp-up throttled), either wait for it
//!    or defer the article back to the queue so a lower-priority dispatch
//!    doesn't pre-empt it.
//! 4. Give up only after every enabled provider explicitly returns NNTP 430.
//!    Transiently unavailable providers remain unresolved; their articles
//!    wait or cascade-retry after provider recovery.
//!
//! This mirrors a classic priority-aware news-retrieval dispatcher: lower
//! priority numbers represent higher-priority (preferred) servers, and an
//! article only falls through to a lower-priority server after every
//! higher-priority server has been exhausted.

use std::sync::Arc;

use tracing::{debug, trace};

use crate::article::{Article, NzbFile, NzbObject};
use crate::server::Server;

/// Outcome of a single selection pass.
#[derive(Debug)]
pub enum Selection<'a> {
    /// A server was selected and is ready to dispatch. The caller should
    /// call [`Server::take_idle_wrapper`] on it and proceed.
    Server(&'a Arc<Server>),
    /// All usable servers are busy right now; the caller should sleep for
    /// the returned duration before re-evaluating. Duration is the shortest
    /// rampup-wait among the viable candidates.
    WaitRampup(std::time::Duration),
    /// Every enabled server has been tried and the article still has
    /// retries remaining. The caller should cascade-reset try-lists and
    /// re-run selection.
    CascadeReset,
    /// The article has exhausted both its server choices and its retry
    /// budget. Mark it failed.
    GiveUp,
}

/// Select the next server for an article given the server list.
///
/// `servers` must be sorted by ascending priority (i.e. highest-priority
/// first — smaller `priority` values first).
pub fn select_server<'a>(
    article: &Article,
    file: &NzbFile,
    job: &NzbObject,
    servers: &'a [Arc<Server>],
) -> Selection<'a> {
    // Priority-aware dispatch: we iterate servers in ascending priority
    // order (highest priority first). A lower-priority server only gets
    // the article if every strictly-higher-priority server is either
    // disabled or already in the article/file/job try-list.
    //
    // Transient unavailability (penalty cooldown, ramp-up throttle,
    // saturation) lets lower-priority servers take over — paying the
    // backup-tier cost is strictly better than idling while the primary
    // recovers. This matches classic priority-aware dispatcher semantics.
    let mut any_candidate = false;
    let mut wait_needed: Option<std::time::Duration> = None;

    for server in servers {
        let id = server.id();

        // Never-dispatchable: skip without affecting candidate counting.
        // "Never" here includes both operator-disabled servers and
        // servers whose wrapper workers have all retired — there is
        // nobody to consume from the queue, so routing there would
        // silently strand the article.
        if !server.is_active() || !server.has_active_wrappers() {
            continue;
        }

        let tried_somewhere =
            article.server_tried(id) || file.server_tried(id) || job.server_tried(id);
        if tried_somewhere {
            // Already failed on this server — a lower-priority server is
            // free to take the article.
            continue;
        }

        // From here the server is a valid candidate for this article —
        // we just need to see whether it can dispatch right now.
        any_candidate = true;

        if server.is_penalised() {
            let remaining = server.penalty_remaining();
            if remaining > std::time::Duration::ZERO {
                wait_needed = Some(match wait_needed {
                    Some(existing) if existing < remaining => existing,
                    _ => remaining,
                });
            }
            trace!(
                server = %id,
                priority = server.priority(),
                "penalised — skipping for this article"
            );
            continue;
        }

        let wait = server.rampup_wait();
        if wait > std::time::Duration::ZERO {
            wait_needed = Some(match wait_needed {
                Some(existing) if existing < wait => existing,
                _ => wait,
            });
            trace!(
                server = %id,
                priority = server.priority(),
                wait_ms = wait.as_millis() as u64,
                "ramp-up gated — skipping for this article"
            );
            continue;
        }

        if server.idle_count() == 0 {
            let poll = std::time::Duration::from_millis(10);
            wait_needed = Some(match wait_needed {
                Some(existing) if existing < poll => existing,
                _ => poll,
            });
            trace!(server = %id, "no idle wrapper — skipping");
            continue;
        }

        // Dispatchable — we traversed every higher-priority server and
        // none of them are dispatchable AND untried. Return this one.
        return Selection::Server(server);
    }

    if !any_candidate {
        let eligible_servers = servers.iter().filter(|server| server.is_active());
        let eligible_count = eligible_servers.clone().count();
        let explicit_global_absence = eligible_count > 0
            && eligible_servers
                .clone()
                .all(|server| article.server_explicitly_not_found(server.id()));

        // Only explicit 430 responses from every enabled provider establish
        // global absence. A provider with no live wrappers, an active
        // penalty, or only transient failures is unresolved and must not
        // turn into permanent content damage.
        if explicit_global_absence {
            debug!(
                article = %article.message_id,
                providers = eligible_count,
                "every eligible provider explicitly returned 430 — giving up"
            );
            return Selection::GiveUp;
        }

        if !article.confirmed_absent_only() {
            debug!(
                article = %article.message_id,
                tries = article.tries(),
                "no candidate servers remain — cascade reset (transient failures seen)"
            );
            return Selection::CascadeReset;
        }

        // At least one eligible provider has not supplied availability
        // evidence and is currently unable to accept work. Wait for worker
        // recovery instead of treating its absence as an article attempt.
        debug!(
            article = %article.message_id,
            tries = article.tries(),
            confirmed_absent_only = article.confirmed_absent_only(),
            "no candidate servers remain, but global absence is unproven — waiting"
        );
        return Selection::WaitRampup(std::time::Duration::from_millis(100));
    }

    // Every candidate is temporarily unavailable (penalty/ramp-up/saturation).
    // Return the shortest wait so the caller can sleep before retrying.
    let wait = wait_needed.unwrap_or(std::time::Duration::from_millis(10));
    Selection::WaitRampup(wait)
}

/// Record that an article has been given up on and can never complete.
///
/// Updates all three levels of bookkeeping: the article's try-list is left
/// populated (as a debugging aid — every server it was tried on is recorded),
/// the parent file ticks its completion counter (so we know when to stop
/// waiting for more articles), and the job's failed-article counter bumps.
///
/// **Does not abort the job.** SAB's model is "one missing article lets the
/// other articles finish, then par2 may be able to repair" — we preserve
/// that here. The downloader continues fetching the remaining articles; the
/// post-processor decides (based on completion ratio + par2 availability)
/// whether the overall job is salvageable.
pub fn record_article_given_up(article: &Article, file: &NzbFile, job: &NzbObject) {
    job.record_article_failed();
    file.mark_article_completed();
    debug!(
        article = %article.message_id,
        file = %file.display_name,
        job = %job.display_name,
        tries = article.tries(),
        file_completed = file.is_complete(),
        "article given up — job continues for par2 repair"
    );
}

/// Cascade-reset: clear the article's try-list first. If the article is
/// already on its last attempt, escalate to resetting the file- and then
/// job-level try-lists. Increments the article's `tries` counter — since
/// the rest of the pipeline no longer bumps `tries` per-fetch, this is
/// the one place where the retry round counter advances, giving us
/// monotonic progress toward `MAX_ARTICLE_TRIES`.
pub fn cascade_reset(article: &Article, file: &NzbFile, job: &NzbObject) {
    article.reset_try_list();
    // When the file itself thinks every server is dead, it's likely the
    // same set of servers is excluded across every article. Reset the
    // file/job-level exclusions so a freshly-enabled or penalty-cleared
    // server becomes a candidate again.
    if !file.try_list().is_empty() {
        file.reset_try_list();
    }
    if !job.try_list().is_empty() {
        job.try_list().reset();
    }
    article.increment_tries();
    debug!(
        article = %article.message_id,
        tries = article.tries(),
        "try-list cascade reset (round advance)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use nzb_nntp::config::ServerConfig;

    fn cfg(id: &str, priority: u8, connections: u16) -> ServerConfig {
        let mut c = ServerConfig::new(id, "host.example");
        c.name = id.into();
        c.priority = priority;
        c.connections = connections;
        c.enabled = true;
        c.port = 119;
        c.ramp_up_delay_ms = 0; // disable throttle for most tests
        c
    }

    fn mkserver(id: &str, priority: u8, connections: u16) -> Arc<Server> {
        let s = Arc::new(Server::new(cfg(id, priority, connections)));
        s.prime_wrapper_pool(connections);
        // Register wrappers so `has_active_wrappers()` — which gates
        // dispatch selection — returns true. Integration tests that
        // drive the Downloader register via `spawn_downloader`; unit
        // tests that poke `select_server` directly must do so here.
        for _ in 0..connections {
            s.register_wrapper();
        }
        s
    }

    fn mkarticle() -> Article {
        Article::new("msg", "file1", "job1", 100_000, 0, 0)
    }

    fn mkfile() -> NzbFile {
        NzbFile::new("file1", "job1", "demo.r00", 10)
    }

    fn mkjob() -> NzbObject {
        NzbObject::new("job1", "demo", 10, 1_000_000, vec![])
    }

    #[test]
    fn picks_highest_priority_server() {
        let primary = mkserver("p", 1, 5);
        let backup = mkserver("b", 5, 5);
        let servers = vec![primary.clone(), backup.clone()];

        let art = mkarticle();
        let file = mkfile();
        let job = mkjob();

        let sel = select_server(&art, &file, &job, &servers);
        match sel {
            Selection::Server(s) => assert_eq!(s.id(), "p"),
            other => panic!("expected primary, got {other:?}"),
        }
    }

    #[test]
    fn falls_through_when_primary_is_tried() {
        let primary = mkserver("p", 1, 5);
        let backup = mkserver("b", 5, 5);
        let servers = vec![primary.clone(), backup.clone()];

        let art = mkarticle();
        art.mark_server_tried("p");
        let file = mkfile();
        let job = mkjob();

        let sel = select_server(&art, &file, &job, &servers);
        match sel {
            Selection::Server(s) => assert_eq!(s.id(), "b"),
            other => panic!("expected backup, got {other:?}"),
        }
    }

    #[test]
    fn cascades_when_all_tried_and_transient_failures_observed() {
        let primary = mkserver("p", 1, 5);
        let backup = mkserver("b", 5, 5);
        let servers = vec![primary.clone(), backup.clone()];

        let art = mkarticle();
        // Simulate one transient failure so cascade is allowed.
        art.mark_transient_failure();
        art.mark_server_tried("p");
        art.mark_server_tried("b");
        let file = mkfile();
        let job = mkjob();

        let sel = select_server(&art, &file, &job, &servers);
        assert!(matches!(sel, Selection::CascadeReset));

        cascade_reset(&art, &file, &job);
        assert!(!art.server_tried("p"));

        // After cascade, selection should succeed on primary again.
        let sel = select_server(&art, &file, &job, &servers);
        match sel {
            Selection::Server(s) => assert_eq!(s.id(), "p"),
            other => panic!("expected primary after cascade, got {other:?}"),
        }
    }

    #[test]
    fn gives_up_without_cascade_when_only_article_not_found() {
        // Every server reported 430 — article is permanently absent.
        // No cascade reset should be offered.
        let primary = mkserver("p", 1, 5);
        let backup = mkserver("b", 5, 5);
        let servers = vec![primary.clone(), backup.clone()];

        let art = mkarticle();
        art.mark_server_not_found("p");
        art.mark_server_not_found("b");
        // Note: no mark_transient_failure — flag stays true by default.
        let file = mkfile();
        let job = mkjob();

        let sel = select_server(&art, &file, &job, &servers);
        assert!(
            matches!(sel, Selection::GiveUp),
            "expected GiveUp for confirmed-absent-only article, got {sel:?}"
        );
    }

    #[test]
    fn transient_failures_do_not_become_absence_after_many_rounds() {
        let primary = mkserver("p", 1, 5);
        let servers = vec![primary.clone()];

        let art = mkarticle();
        art.mark_server_tried("p");
        art.mark_transient_failure();
        for _ in 0..10 {
            art.increment_tries();
        }
        let file = mkfile();
        let job = mkjob();

        let sel = select_server(&art, &file, &job, &servers);
        assert!(matches!(sel, Selection::CascadeReset));
    }

    #[test]
    fn unavailable_untried_provider_prevents_global_absence() {
        let primary = mkserver("p", 1, 5);
        let backup = mkserver("b", 5, 5);
        backup.set_active(true);
        for _ in 0..backup.active_wrappers() {
            backup.unregister_wrapper();
        }
        let servers = vec![primary, backup];

        let art = mkarticle();
        art.mark_server_not_found("p");
        let file = mkfile();
        let job = mkjob();

        assert!(matches!(
            select_server(&art, &file, &job, &servers),
            Selection::WaitRampup(_)
        ));
    }

    #[test]
    fn skips_penalised_server_but_can_fall_back() {
        let primary = mkserver("p", 1, 5);
        let backup = mkserver("b", 5, 5);
        primary.apply_penalty(std::time::Duration::from_secs(5));
        let servers = vec![primary.clone(), backup.clone()];

        let art = mkarticle();
        let file = mkfile();
        let job = mkjob();

        let sel = select_server(&art, &file, &job, &servers);
        match sel {
            Selection::Server(s) => assert_eq!(s.id(), "b"),
            other => panic!("expected backup when primary is penalised, got {other:?}"),
        }
    }

    #[test]
    fn skips_file_level_try_list() {
        let primary = mkserver("p", 1, 5);
        let backup = mkserver("b", 5, 5);
        let servers = vec![primary.clone(), backup.clone()];

        let art = mkarticle();
        let file = mkfile();
        file.mark_server_tried("p"); // file says primary is useless
        let job = mkjob();

        let sel = select_server(&art, &file, &job, &servers);
        match sel {
            Selection::Server(s) => assert_eq!(s.id(), "b"),
            other => panic!("expected backup when file excludes primary, got {other:?}"),
        }
    }

    #[test]
    fn falls_through_when_primary_rampup_throttled() {
        // Rampup throttle is a per-server spacing mechanism, not a hold.
        // Lower-priority servers must be allowed to take the article so a
        // temporarily-throttled primary doesn't idle the job.
        let mut c = cfg("p", 1, 5);
        c.ramp_up_delay_ms = 1_000;
        let primary = Arc::new(Server::new(c));
        primary.prime_wrapper_pool(5);
        for _ in 0..5 {
            primary.register_wrapper();
        }
        let _w = primary.take_idle_wrapper().expect("first go");

        let backup = mkserver("b", 5, 5);
        let servers = vec![primary.clone(), backup.clone()];

        let art = mkarticle();
        let file = mkfile();
        let job = mkjob();

        let sel = select_server(&art, &file, &job, &servers);
        match sel {
            Selection::Server(s) => assert_eq!(s.id(), "b"),
            other => panic!("expected fall-through to backup, got {other:?}"),
        }
    }

    #[test]
    fn waits_when_every_candidate_is_transiently_blocked() {
        // With only one server, and it is in rampup — we have nowhere else
        // to go. Selection must return WaitRampup (not CascadeReset,
        // because the server hasn't been tried).
        let mut c = cfg("p", 1, 5);
        c.ramp_up_delay_ms = 500;
        let primary = Arc::new(Server::new(c));
        primary.prime_wrapper_pool(5);
        for _ in 0..5 {
            primary.register_wrapper();
        }
        let _w = primary.take_idle_wrapper().expect("first go");

        let servers = vec![primary.clone()];
        let art = mkarticle();
        let file = mkfile();
        let job = mkjob();

        let sel = select_server(&art, &file, &job, &servers);
        match sel {
            Selection::WaitRampup(d) => assert!(d > std::time::Duration::ZERO),
            other => panic!("expected WaitRampup, got {other:?}"),
        }
    }

    #[test]
    fn give_up_marks_failed_and_ticks_completion() {
        let art = mkarticle();
        let f = Arc::new(mkfile());
        let job = NzbObject::new("job1", "demo", 3, 1_000_000, vec![f.clone()]);
        record_article_given_up(&art, &f, &job);
        assert_eq!(job.articles_failed(), 1);
        // File tracks completion so is_complete() will be true once all
        // three articles have a terminal outcome — here only one of three.
        assert!(!f.is_complete());
    }

    #[test]
    fn cascade_reset_clears_all_levels() {
        let art = mkarticle();
        let file = mkfile();
        let job = mkjob();
        art.mark_server_tried("s1");
        file.mark_server_tried("s1");
        job.mark_server_tried("s1");

        cascade_reset(&art, &file, &job);

        assert!(!art.server_tried("s1"));
        assert!(!file.server_tried("s1"));
        assert!(!job.server_tried("s1"));
    }
}
