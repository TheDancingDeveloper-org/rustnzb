//! Public-surface tests for the newsgroup browsing database (`groups_db`).
//!
//! The module only compiles with the `groups-db` feature, so this whole file
//! is gated on it. Run with `cargo test -p nzb-core --features groups-db`.
#![cfg(feature = "groups-db")]

use nzb_core::db::Database;
use nzb_nntp::XoverEntry;

fn entry(article_num: u64, message_id: &str, references: &str, subject: &str) -> XoverEntry {
    XoverEntry {
        article_num,
        subject: subject.into(),
        from: "poster".into(),
        date: format!("2026-01-01 {article_num:08}"),
        message_id: message_id.into(),
        references: references.into(),
        bytes: 1,
        lines: 1,
    }
}

fn group_id(db: &Database, name: &str) -> i64 {
    db.group_list(false, Some(name), 10, 0)
        .unwrap()
        .into_iter()
        .find(|g| g.name == name)
        .unwrap_or_else(|| panic!("group {name} missing"))
        .id
}

fn seed(db: &Database, names: &[&str]) {
    let groups: Vec<(String, u64, u64)> = names
        .iter()
        .map(|name| ((*name).to_string(), 30, 10))
        .collect();
    db.group_upsert_batch(&groups).unwrap();
}

#[test]
fn upsert_refreshes_counts_of_an_existing_group_without_duplicating_it() {
    let db = Database::open_memory().unwrap();
    assert_eq!(
        db.group_upsert_batch(&[("alt.binaries.test".into(), 30, 10)])
            .unwrap(),
        1
    );
    let first = db.group_list(false, None, 10, 0).unwrap().pop().unwrap();
    assert_eq!(first.article_count, 20);
    assert_eq!(first.first_article, 10);
    assert_eq!(first.last_article, 30);

    db.group_upsert_batch(&[("alt.binaries.test".into(), 50, 40)])
        .unwrap();

    let groups = db.group_list(false, None, 10, 0).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].id, first.id, "upsert must keep the row id");
    assert_eq!(groups[0].article_count, 10);
    assert_eq!(groups[0].first_article, 40);
    assert_eq!(groups[0].last_article, 50);
}

#[test]
fn group_list_filters_by_subscription_and_case_insensitive_name() {
    let db = Database::open_memory().unwrap();
    seed(
        &db,
        &["alt.binaries.linux", "alt.binaries.music", "comp.lang.rust"],
    );
    db.group_set_subscribed(group_id(&db, "alt.binaries.linux"), true)
        .unwrap();

    let subscribed: Vec<String> = db
        .group_list(true, None, 10, 0)
        .unwrap()
        .into_iter()
        .map(|g| g.name)
        .collect();
    assert_eq!(subscribed, ["alt.binaries.linux"]);
    assert_eq!(db.group_count(true, None).unwrap(), 1);

    let binaries = db.group_list(false, Some("BINARIES"), 10, 0).unwrap();
    assert_eq!(binaries.len(), 2);
    assert_eq!(db.group_count(false, Some("BINARIES")).unwrap(), 2);

    let none = db.group_list(false, Some("no.such.group"), 10, 0).unwrap();
    assert!(none.is_empty());
    assert_eq!(db.group_count(false, Some("no.such.group")).unwrap(), 0);
}

#[test]
fn group_list_pages_in_name_order() {
    let db = Database::open_memory().unwrap();
    seed(&db, &["e.g", "a.b", "d.f", "b.c", "c.d"]);

    assert_eq!(db.group_count(false, None).unwrap(), 5);
    let page: Vec<String> = db
        .group_list(false, None, 2, 2)
        .unwrap()
        .into_iter()
        .map(|g| g.name)
        .collect();
    assert_eq!(page, ["c.d", "d.f"]);

    let tail = db.group_list(false, None, 10, 4).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].name, "e.g");
}

#[test]
fn group_subscription_toggles_and_missing_group_reads_as_none() {
    let db = Database::open_memory().unwrap();
    seed(&db, &["alt.test"]);
    let id = group_id(&db, "alt.test");

    db.group_set_subscribed(id, true).unwrap();
    assert!(db.group_get(id).unwrap().unwrap().subscribed);
    db.group_set_subscribed(id, false).unwrap();
    assert!(!db.group_get(id).unwrap().unwrap().subscribed);

    assert!(db.group_get(id + 1_000).unwrap().is_none());
}

#[test]
fn scan_watermark_and_its_server_round_trip_and_clear() {
    let db = Database::open_memory().unwrap();
    seed(&db, &["alt.test"]);
    let id = group_id(&db, "alt.test");
    assert_eq!(db.group_scan_server(id).unwrap(), None);

    db.group_update_watermark(id, 42, Some("primary")).unwrap();
    assert_eq!(db.group_get(id).unwrap().unwrap().last_scanned, 42);
    assert_eq!(
        db.group_scan_server(id).unwrap().as_deref(),
        Some("primary")
    );

    db.group_update_watermark(id, 57, None).unwrap();
    assert_eq!(db.group_get(id).unwrap().unwrap().last_scanned, 57);
    assert_eq!(db.group_scan_server(id).unwrap(), None);

    assert_eq!(db.group_scan_server(id + 1_000).unwrap(), None);
}

#[test]
fn mark_all_read_only_touches_the_named_group() {
    let db = Database::open_memory().unwrap();
    seed(&db, &["alt.one", "alt.two"]);
    let one = group_id(&db, "alt.one");
    let two = group_id(&db, "alt.two");
    db.header_insert_batch(
        one,
        &[entry(10, "a@test", "", "A"), entry(11, "b@test", "", "B")],
    )
    .unwrap();
    db.header_insert_batch(two, &[entry(10, "c@test", "", "C")])
        .unwrap();

    assert_eq!(db.header_mark_all_read(one).unwrap(), 2);
    assert_eq!(db.header_unread_count(one).unwrap(), 0);
    assert_eq!(db.header_unread_count(two).unwrap(), 1);
    assert_eq!(db.header_mark_all_read(one).unwrap(), 0);
    assert_eq!(db.header_mark_all_read(one + 1_000).unwrap(), 0);
}

#[test]
fn header_lookup_by_message_id_finds_the_stored_article() {
    let db = Database::open_memory().unwrap();
    seed(&db, &["alt.test"]);
    let id = group_id(&db, "alt.test");
    db.header_insert_batch(id, &[entry(12, "wanted@test", "", "Wanted")])
        .unwrap();

    let found = db
        .header_get_by_message_id("wanted@test")
        .unwrap()
        .expect("stored header");
    assert_eq!(found.group_id, id);
    assert_eq!(found.article_num, 12);
    assert_eq!(found.subject, "Wanted");
    assert!(!found.read);

    assert!(
        db.header_get_by_message_id("absent@test")
            .unwrap()
            .is_none()
    );
}

#[test]
fn header_list_pages_newest_first() {
    let db = Database::open_memory().unwrap();
    seed(&db, &["alt.test"]);
    let id = group_id(&db, "alt.test");
    let entries: Vec<XoverEntry> = (10..15)
        .map(|n| entry(n, &format!("m{n}@test"), "", &format!("S{n}")))
        .collect();
    db.header_insert_batch(id, &entries).unwrap();

    assert_eq!(db.header_count(id, None).unwrap(), 5);
    let page: Vec<i64> = db
        .header_list(id, None, 2, 1)
        .unwrap()
        .into_iter()
        .map(|h| h.article_num)
        .collect();
    assert_eq!(page, [13, 12]);
}

#[test]
fn thread_list_pages_by_most_recent_reply_with_exact_totals() {
    let db = Database::open_memory().unwrap();
    seed(&db, &["alt.test"]);
    let id = group_id(&db, "alt.test");
    db.header_insert_batch(
        id,
        &[
            entry(10, "t1@test", "", "Thread one"),
            entry(11, "t2@test", "", "Thread two"),
            entry(12, "t3@test", "", "Thread three"),
        ],
    )
    .unwrap();

    let (first, total) = db.header_list_threads(id, 2, 0).unwrap();
    assert_eq!(total, 3);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].root_message_id, "t3@test");
    assert_eq!(first[1].root_message_id, "t2@test");

    let (last, total) = db.header_list_threads(id, 2, 2).unwrap();
    assert_eq!(total, 3);
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].root_message_id, "t1@test");
}

#[test]
fn thread_article_depth_counts_references() {
    let db = Database::open_memory().unwrap();
    seed(&db, &["alt.test"]);
    let id = group_id(&db, "alt.test");
    db.header_insert_batch(
        id,
        &[
            entry(10, "root@test", "", "Root"),
            entry(11, "reply@test", "root@test", "Re: Root"),
            entry(12, "deep@test", "root@test reply@test", "Re: Re: Root"),
        ],
    )
    .unwrap();

    let thread = db.header_get_thread(id, "root@test").unwrap();
    let depths: Vec<(String, i32)> = thread
        .iter()
        .map(|a| (a.header.message_id.clone(), a.depth))
        .collect();
    assert_eq!(
        depths,
        [
            ("deep@test".to_string(), 2),
            ("reply@test".to_string(), 1),
            ("root@test".to_string(), 0),
        ]
    );

    assert!(db.header_get_thread(id, "unknown@test").unwrap().is_empty());
}
