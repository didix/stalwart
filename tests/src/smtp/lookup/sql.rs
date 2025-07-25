/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::time::{Duration, Instant};

use common::{
    Core,
    expr::{tokenizer::TokenMap, *},
};

use directory::{
    QueryBy, Type,
    backend::internal::{PrincipalField, PrincipalSet, PrincipalValue, manage::ManageDirectory},
};
use mail_auth::MX;
use store::Stores;
use utils::config::Config;

use crate::{
    directory::DirectoryStore,
    smtp::{
        DnsCache, TempDir, TestSMTP,
        session::{TestSession, VerifyResponse},
    },
};
use smtp::{core::Session, queue::RecipientDomain};

const CONFIG: &str = r#"
[storage]
data = "sql"
blob = "sql"
fts = "sql"
lookup = "sql"
directory = "sql"

[store."sql"]
type = "sqlite"
path = "{TMP}/smtp_sql.db"

[store."sql".query]
name = "SELECT name, type, secret, description, quota FROM accounts WHERE name = ? AND active = true"
members = "SELECT member_of FROM group_members WHERE name = ?"
recipients = "SELECT name FROM emails WHERE address = ?"
emails = "SELECT address FROM emails WHERE name = ? AND type != 'list' ORDER BY type DESC, address ASC"
verify = "SELECT address FROM emails WHERE address LIKE '%' || ? || '%' AND type = 'primary' ORDER BY address LIMIT 5"
expand = "SELECT p.address FROM emails AS p JOIN emails AS l ON p.name = l.name WHERE p.type = 'primary' AND l.address = ? AND l.type = 'list' ORDER BY p.address LIMIT 50"
domains = "SELECT 1 FROM emails WHERE address LIKE '%@' || ? LIMIT 1"

[directory."sql"]
type = "sql"
store = "sql"

[directory."sql".columns]
name = "name"
description = "description"
secret = "secret"
email = "address"
quota = "quota"
class = "type"

[session.auth]
directory = "'sql'"
mechanisms = "[plain, login]"
errors.wait = "5ms"

[session.rcpt]
directory = "'sql'"
relay = false
errors.wait = "5ms"

[session.extensions]
requiretls = [{if = "sql_query('sql', 'SELECT addr FROM allowed_ips WHERE addr = ? LIMIT 1', remote_ip)", then = true},
              {else = false}]
expn = true
vrfy = true

[test."sql"]
expr = "sql_query('sql', 'SELECT description FROM domains WHERE name = ?', 'foobar.org')"
expect = "Main domain"

[test."dns"]
expr = "dns_query(rcpt_domain, 'mx')[0]"
expect = "mx.foobar.org"

[test."key_get"]
expr = "key_get('sql', 'hello') + '-' + key_exists('sql', 'hello') + '-' + key_set('sql', 'hello', 'world') + '-' + key_get('sql', 'hello') + '-' + key_exists('sql', 'hello')"
expect = "-0-1-world-1"

[test."counter_get"]
expr = "counter_get('sql', 'county') + '-' + counter_incr('sql', 'county', 1) + '-' + counter_incr('sql', 'county', 1) + '-' + counter_get('sql', 'county')"
expect = "0-1-2-2"

"#;

#[tokio::test]
async fn lookup_sql() {
    // Enable logging
    crate::enable_logging();

    // Parse settings
    let temp_dir = TempDir::new("smtp_lookup_tests", true);
    let mut config = Config::new(temp_dir.update_config(CONFIG)).unwrap();
    let stores = Stores::parse_all(&mut config, false).await;

    let core = Core::parse(&mut config, stores, Default::default()).await;

    // Obtain directory handle
    let handle = DirectoryStore {
        store: core.storage.stores.get("sql").unwrap().clone(),
    };
    let test = TestSMTP::from_core(core);

    test.server.mx_add(
        "test.org",
        vec![MX {
            exchanges: vec!["mx.foobar.org".to_string()],
            preference: 10,
        }],
        Instant::now() + Duration::from_secs(10),
    );

    // Create tables
    handle.create_test_directory().await;

    // Create test records
    handle
        .create_test_user_with_email("jane@foobar.org", "s3cr3tp4ss", "Jane")
        .await;
    handle
        .create_test_user_with_email("john@foobar.org", "mypassword", "John")
        .await;
    handle
        .create_test_user_with_email("bill@foobar.org", "123456", "Bill")
        .await;
    handle
        .create_test_user_with_email("mike@foobar.net", "098765", "Mike")
        .await;

    for query in [
        "CREATE TABLE domains (name TEXT PRIMARY KEY, description TEXT);",
        "INSERT INTO domains (name, description) VALUES ('foobar.org', 'Main domain');",
        "INSERT INTO domains (name, description) VALUES ('foobar.net', 'Secondary domain');",
        "CREATE TABLE allowed_ips (addr TEXT PRIMARY KEY);",
        "INSERT INTO allowed_ips (addr) VALUES ('10.0.0.50');",
    ] {
        handle
            .store
            .sql_query::<usize>(query, Vec::new())
            .await
            .unwrap();
    }

    // Create local domains
    let internal_store = &test.server.core.storage.data;
    for name in ["foobar.org", "foobar.net"] {
        internal_store
            .create_principal(
                PrincipalSet::new(0, Type::Domain).with_field(PrincipalField::Name, name),
                None,
                None,
            )
            .await
            .unwrap();
    }

    // Create lists
    internal_store
        .create_principal(
            PrincipalSet::new(0, Type::List)
                .with_field(PrincipalField::Name, "support@foobar.org")
                .with_field(PrincipalField::Emails, "support@foobar.org")
                .with_field(
                    PrincipalField::ExternalMembers,
                    PrincipalValue::StringList(vec!["mike@foobar.net".to_string()]),
                ),
            None,
            None,
        )
        .await
        .unwrap();
    internal_store
        .create_principal(
            PrincipalSet::new(0, Type::List)
                .with_field(PrincipalField::Name, "sales@foobar.org")
                .with_field(PrincipalField::Emails, "sales@foobar.org")
                .with_field(
                    PrincipalField::ExternalMembers,
                    PrincipalValue::StringList(vec![
                        "jane@foobar.org".to_string(),
                        "john@foobar.org".to_string(),
                        "bill@foobar.org".to_string(),
                    ]),
                ),
            None,
            None,
        )
        .await
        .unwrap();

    // Test expression functions
    let token_map = TokenMap::default().with_variables(&[
        V_RECIPIENT,
        V_RECIPIENT_DOMAIN,
        V_SENDER,
        V_SENDER_DOMAIN,
        V_MX,
        V_HELO_DOMAIN,
        V_AUTHENTICATED_AS,
        V_LISTENER,
        V_REMOTE_IP,
        V_LOCAL_IP,
        V_PRIORITY,
    ]);
    for test_name in ["sql", "dns", "key_get", "counter_get"] {
        let e =
            Expression::try_parse(&mut config, ("test", test_name, "expr"), &token_map).unwrap();
        assert_eq!(
            test.server
                .eval_expr::<String, _>(&e, &RecipientDomain::new("test.org"), "text", 0)
                .await
                .unwrap(),
            config.value(("test", test_name, "expect")).unwrap(),
            "failed for '{}'",
            test_name
        );
    }

    let mut session = Session::test(test.server);
    session.data.remote_ip_str = "10.0.0.50".parse().unwrap();
    session.eval_session_params().await;
    session.stream.tls = true;
    session
        .ehlo("mx.foobar.org")
        .await
        .assert_contains("REQUIRETLS");
    session.data.remote_ip_str = "10.0.0.1".into();
    session.eval_session_params().await;
    session
        .ehlo("mx1.foobar.org")
        .await
        .assert_not_contains("REQUIRETLS");

    // Test RCPT
    session.mail_from("john@example.net", "250").await;

    // External domain
    session.rcpt_to("user@otherdomain.org", "550 5.1.2").await;

    // Non-existent user
    session.rcpt_to("jack@foobar.org", "550 5.1.2").await;

    // Valid users
    session.rcpt_to("jane@foobar.org", "250").await;
    session.rcpt_to("john@foobar.org", "250").await;
    session.rcpt_to("bill@foobar.org", "250").await;

    // Lists
    session.rcpt_to("sales@foobar.org", "250").await;

    // Test EXPN
    session
        .cmd("EXPN sales@foobar.org", "250")
        .await
        .assert_contains("jane@foobar.org")
        .assert_contains("john@foobar.org")
        .assert_contains("bill@foobar.org");
    session
        .cmd("EXPN support@foobar.org", "250")
        .await
        .assert_contains("mike@foobar.net");
    session.cmd("EXPN marketing@foobar.org", "550 5.1.2").await;

    // Test VRFY
    session
        .server
        .core
        .storage
        .directory
        .query(QueryBy::Name("john@foobar.org"), true)
        .await
        .unwrap()
        .unwrap();
    session
        .server
        .core
        .storage
        .directory
        .query(QueryBy::Name("jane@foobar.org"), true)
        .await
        .unwrap()
        .unwrap();
    session
        .cmd("VRFY john", "250")
        .await
        .assert_contains("john@foobar.org");
    session
        .cmd("VRFY jane", "250")
        .await
        .assert_contains("jane@foobar.org");
    session.cmd("VRFY tim", "550 5.1.2").await;

    // Test AUTH
    session
        .cmd(
            "AUTH PLAIN AGphbmVAZm9vYmFyLm9yZwB3cm9uZ3Bhc3M=",
            "535 5.7.8",
        )
        .await;
    session
        .cmd(
            "AUTH PLAIN AGphbmVAZm9vYmFyLm9yZwBzM2NyM3RwNHNz",
            "235 2.7.0",
        )
        .await;
}
