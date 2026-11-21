use exodys::{
    catalog::{MAX_AUTO_MEMBERS, NodeRef, PolicyMode},
    import::Source,
    session::{Job, Session},
};

fn import(session: &mut Session, content: String) -> Vec<NodeRef> {
    let profile = session
        .run(Job::Import(Source::Text(content)))
        .unwrap()
        .focus
        .unwrap();
    session
        .snapshot()
        .catalog
        .profile(profile)
        .unwrap()
        .nodes
        .iter()
        .map(|node| NodeRef {
            profile,
            node: node.id.clone(),
        })
        .collect()
}

#[test]
fn group_is_explicit_cross_profile_and_durable_without_manual_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::open(dir.path().into()).unwrap();
    let a = import(&mut s, "socks5://localhost:1080#Same".into()).remove(0);
    let b = import(&mut s, "socks5://localhost:1081#Same".into()).remove(0);
    let manual = import(&mut s, "socks5://localhost:1082#Manual".into()).remove(0);
    s.run(Job::Select(manual.clone())).unwrap();
    assert!(s.run(Job::SetAutoEnabled(true)).is_err());
    for m in [&a, &b, &a] {
        s.run(Job::SetAutoMember(m.clone(), true)).unwrap();
    }
    s.run(Job::SetAutoEnabled(true)).unwrap();
    drop(s);
    let mut s = Session::open(dir.path().into()).unwrap();
    let prefs = s.snapshot().preferences;
    assert!(prefs.auto_enabled);
    assert_eq!(prefs.auto_members, [a.clone(), b.clone()]);
    assert_eq!(prefs.selected, Some(manual.clone()));
    let mut running = s.snapshot();
    running.connected = true;
    assert!(running.profile_in_use(a.profile) && running.profile_in_use(b.profile));
    assert!(!running.profile_in_use(manual.profile));
    s.run(Job::Remove(a.profile)).unwrap();
    assert!(s.run(Job::SetAutoMember(b.clone(), false)).is_err());
    s.run(Job::Remove(b.profile)).unwrap();
    drop(s);
    let mut s = Session::open(dir.path().into()).unwrap();
    assert!(
        s.snapshot().preferences.auto_enabled && s.snapshot().preferences.auto_members.is_empty()
    );
    assert!(s.run(Job::Connect).err().unwrap().contains("没有可用节点"));
    assert_eq!(s.snapshot().preferences.selected, Some(manual));
    s.run(Job::SetAutoEnabled(false)).unwrap();
    assert!(s.run(Job::SetAutoMember(a, true)).is_err());
}

#[test]
fn independent_policies_persist_and_protect_saved_routes() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path().into()).unwrap();
    let a = import(&mut session, "socks5://localhost:1080#One".into()).remove(0);
    let b = import(&mut session, "socks5://localhost:1081#Two".into()).remove(0);
    let rules = "DOMAIN,a.test,GROUP:office\nMATCH,GROUP:home";
    assert!(session.run(Job::SetRouting(rules.into())).is_err());
    for group in ["office", "home"] {
        session.run(Job::CreatePolicy(group.into())).unwrap();
        for member in [&a, &b] {
            session
                .run(Job::SetPolicyMember(group.into(), member.clone(), true))
                .unwrap();
        }
    }
    session
        .run(Job::SetPolicyMode(
            "office".into(),
            PolicyMode::Manual(Some(a.clone())),
        ))
        .unwrap();
    session
        .run(Job::SetPolicyMode("home".into(), PolicyMode::Automatic))
        .unwrap();
    session.run(Job::SetRouting(rules.into())).unwrap();
    assert!(session.run(Job::RemovePolicy("home".into())).is_err());
    drop(session);

    let mut session = Session::open(dir.path().into()).unwrap();
    assert_eq!(session.snapshot().preferences.routing_rules, rules);
    assert_eq!(
        session.snapshot().preferences.policy_groups[0].mode,
        PolicyMode::Manual(Some(a.clone()))
    );
    session.run(Job::Remove(a.profile)).unwrap();
    let prefs = session.snapshot().preferences;
    assert_eq!(prefs.policy_groups[0].mode, PolicyMode::Manual(None));
    assert_eq!(prefs.policy_groups[0].members, std::slice::from_ref(&b));
    assert_eq!(prefs.policy_groups[1].mode, PolicyMode::Automatic);
    assert_eq!(prefs.policy_groups[1].members, [b]);
    assert!(session.run(Job::Connect).err().unwrap().contains("office"));
}

#[test]
fn too_many_candidates_preserves_the_last_good_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::open(dir.path().into()).unwrap();
    let links = (0..=MAX_AUTO_MEMBERS)
        .map(|i| format!("socks5://localhost:{}#relay{i}", 10000 + i))
        .collect::<Vec<_>>()
        .join("\n");
    let nodes = import(&mut s, links);
    for node in &nodes[..MAX_AUTO_MEMBERS] {
        s.run(Job::SetAutoMember(node.clone(), true)).unwrap();
    }
    assert!(
        s.run(Job::SetAutoMember(nodes[MAX_AUTO_MEMBERS].clone(), true))
            .is_err()
    );
    assert_eq!(
        s.snapshot().preferences.auto_members,
        nodes[..MAX_AUTO_MEMBERS]
    );
}
