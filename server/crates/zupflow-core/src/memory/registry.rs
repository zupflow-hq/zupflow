//! [`MemoryProviders`]: the memory kinds a server serves, by key. A type is
//! served by the provider of the kind it was created with; nothing picks one
//! kind for the whole server.
//!
//! A stored type whose kind this server doesn't serve answers through a
//! stand-in, so callers need no special case: its agents run without memory
//! (no tools, no recall), and everything else it's asked answers
//! `NotServed`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::{
    Audience, CallContext, CallResult, Forget, KindDescription, Manage, Memory, MemoryError, MemoryFilter, MemoryKind,
    MemoryProvider, MemoryScope, MemoryTool, MemoryWrites, Origin, Recall, Record, Rewritten, Settings,
    SettingsRefusal,
};
use crate::audit::Audit;
use crate::ids::MemoryId;
use crate::time::Timestamp;
use crate::workspaces::Page;

/// The memory kinds a server serves, by key. Cheap to clone: clones share
/// the same providers.
#[derive(Clone, Default)]
pub struct MemoryProviders {
    kinds: BTreeMap<MemoryKind, Arc<dyn MemoryProvider>>,
}

impl std::fmt::Debug for MemoryProviders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryProviders")
            .field("served", &self.kinds.keys().map(MemoryKind::as_str).collect::<Vec<_>>())
            .finish()
    }
}

impl MemoryProviders {
    /// None: every agent runs without memory.
    pub fn none() -> Self {
        Self::default()
    }

    /// Adds `provider` for its kind, replacing one of that kind.
    #[must_use]
    pub fn with(mut self, provider: Arc<dyn MemoryProvider>) -> Self {
        self.kinds.insert(provider.kind().clone(), provider);
        self
    }

    /// The kinds served, by key.
    pub fn served(&self) -> Vec<MemoryKind> {
        self.kinds.keys().cloned().collect()
    }

    pub fn serves(&self, kind: &MemoryKind) -> bool {
        self.kinds.contains_key(kind)
    }

    /// `kind`'s provider: when this server doesn't serve it, a stand-in whose
    /// agents run without memory.
    pub fn provider(&self, kind: &MemoryKind) -> Arc<dyn MemoryProvider> {
        match self.kinds.get(kind) {
            Some(provider) => provider.clone(),
            None => Arc::new(Unserved { kind: kind.clone() }),
        }
    }

    /// Every provider served, by key: what forgetting an agent, a workspace
    /// or an organization asks.
    pub fn providers(&self) -> Vec<Arc<dyn MemoryProvider>> {
        self.kinds.values().cloned().collect()
    }

    /// What `listMemoryKinds` lists: each served kind's description, by key.
    pub fn descriptions(&self) -> Vec<KindDescription> {
        self.kinds.values().map(|provider| provider.describe()).collect()
    }
}

/// The provider of a kind this server doesn't serve.
struct Unserved {
    kind: MemoryKind,
}

#[async_trait]
impl MemoryProvider for Unserved {
    fn kind(&self) -> &MemoryKind {
        &self.kind
    }

    fn describe(&self) -> KindDescription {
        KindDescription {
            kind: self.kind.clone(),
            name: self.kind.as_str().to_owned(),
            description: "Not served here: its agents run without memory.".to_owned(),
            settings_schema: json!({ "type": "object" }),
            manage: Manage {
                list: false,
                add: false,
                change: false,
                forget: false,
            },
        }
    }

    /// Keeps the settings as stored: a kind it doesn't serve can't check them.
    fn check_settings(&self, settings: &Map<String, Value>) -> Result<Settings, SettingsRefusal> {
        Ok(Settings::checked(settings.clone()))
    }

    fn tools(&self, _: &Settings, _: MemoryWrites) -> Vec<MemoryTool> {
        Vec::new()
    }

    fn text_rule(&self) -> String {
        format!("This server doesn't serve {}, so it keeps no text.", self.kind)
    }

    /// It keeps none.
    fn limit(&self, _: &Settings) -> u32 {
        0
    }

    async fn recall(&self, _: &MemoryScope, _: &Audience, _: &Settings) -> Result<Recall, MemoryError> {
        Ok(Recall::none())
    }

    async fn call(
        &self,
        _: &MemoryScope,
        _: &CallContext<'_>,
        _: &str,
        _: &Map<String, Value>,
    ) -> Result<CallResult, MemoryError> {
        Err(MemoryError::NotServed)
    }

    async fn list(&self, _: &MemoryScope, _: &MemoryFilter) -> Result<Page<Memory>, MemoryError> {
        Err(MemoryError::NotServed)
    }

    async fn get(&self, _: &MemoryScope, _: MemoryId) -> Result<Memory, MemoryError> {
        Err(MemoryError::NotServed)
    }

    async fn add(
        &self,
        _: &MemoryScope,
        _: MemoryId,
        _: &Origin,
        _: &Settings,
        _: &str,
        _: Timestamp,
        _: &Record<'_>,
    ) -> Result<Memory, MemoryError> {
        Err(MemoryError::NotServed)
    }

    async fn change(
        &self,
        _: &MemoryScope,
        _: &Origin,
        _: MemoryId,
        _: &str,
        _: Timestamp,
        _: &Record<'_>,
    ) -> Result<Rewritten, MemoryError> {
        Err(MemoryError::NotServed)
    }

    async fn forget(&self, _: &MemoryScope, _: MemoryId, _: &Record<'_>) -> Result<Memory, MemoryError> {
        Err(MemoryError::NotServed)
    }

    async fn forget_scope(
        &self,
        _: &MemoryScope,
        _: u32,
        _: &Record<'_>,
        _: &Audit<'_, u64>,
    ) -> Result<u64, MemoryError> {
        Err(MemoryError::NotServed)
    }

    async fn forget_all(&self, _: &Forget) -> Result<(), MemoryError> {
        Err(MemoryError::NotServed)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::audit::AuditEntry;
    use crate::ids::{AgentId, MemoryTypeId, OrganizationId, UserId, WorkspaceId};
    use crate::memory::record_nothing;

    /// A count's audit event, for a kind that forgets nothing.
    fn never_counted(_: &u64) -> AuditEntry {
        unreachable!("nothing is forgotten, so nothing is counted")
    }

    /// A kind that names itself in its description, and keeps nothing.
    struct Named {
        kind: MemoryKind,
        name: &'static str,
    }

    #[async_trait]
    impl MemoryProvider for Named {
        fn kind(&self) -> &MemoryKind {
            &self.kind
        }
        fn describe(&self) -> KindDescription {
            KindDescription {
                name: self.name.to_owned(),
                ..Unserved {
                    kind: self.kind.clone(),
                }
                .describe()
            }
        }
        fn check_settings(&self, _: &Map<String, Value>) -> Result<Settings, SettingsRefusal> {
            Ok(Settings::default())
        }
        fn tools(&self, _: &Settings, _: MemoryWrites) -> Vec<MemoryTool> {
            Vec::new()
        }
        fn text_rule(&self) -> String {
            self.name.to_owned()
        }
        fn limit(&self, _: &Settings) -> u32 {
            0
        }
        async fn recall(&self, _: &MemoryScope, _: &Audience, _: &Settings) -> Result<Recall, MemoryError> {
            Ok(Recall::of_lines([self.name.to_owned()]))
        }
        async fn call(
            &self,
            _: &MemoryScope,
            _: &CallContext<'_>,
            _: &str,
            _: &Map<String, Value>,
        ) -> Result<CallResult, MemoryError> {
            Ok(CallResult::answer(self.name, None))
        }
        async fn list(&self, _: &MemoryScope, _: &MemoryFilter) -> Result<Page<Memory>, MemoryError> {
            Err(MemoryError::Unsupported)
        }
        async fn get(&self, _: &MemoryScope, _: MemoryId) -> Result<Memory, MemoryError> {
            Err(MemoryError::NotFound)
        }
        async fn add(
            &self,
            _: &MemoryScope,
            _: MemoryId,
            _: &Origin,
            _: &Settings,
            _: &str,
            _: Timestamp,
            _: &Record<'_>,
        ) -> Result<Memory, MemoryError> {
            Err(MemoryError::Unsupported)
        }
        async fn change(
            &self,
            _: &MemoryScope,
            _: &Origin,
            _: MemoryId,
            _: &str,
            _: Timestamp,
            _: &Record<'_>,
        ) -> Result<Rewritten, MemoryError> {
            Err(MemoryError::Unsupported)
        }
        async fn forget(&self, _: &MemoryScope, _: MemoryId, _: &Record<'_>) -> Result<Memory, MemoryError> {
            Err(MemoryError::Unsupported)
        }
        async fn forget_scope(
            &self,
            _: &MemoryScope,
            _: u32,
            _: &Record<'_>,
            _: &Audit<'_, u64>,
        ) -> Result<u64, MemoryError> {
            Err(MemoryError::Unsupported)
        }
        async fn forget_all(&self, _: &Forget) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    fn kind(key: &str) -> MemoryKind {
        MemoryKind::parse(key).expect("a kind's form")
    }

    fn scope() -> MemoryScope {
        MemoryScope {
            organization_id: OrganizationId::from_uuid(Uuid::from_u128(1)),
            workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(2)),
            agent_id: AgentId::from_uuid(Uuid::from_u128(3)),
            memory_type_id: MemoryTypeId::from_uuid(Uuid::from_u128(4)),
        }
    }

    #[tokio::test]
    async fn a_kind_not_served_runs_without_memory() {
        let none = MemoryProviders::none();
        let notes = kind("notes");
        assert!(none.served().is_empty() && none.descriptions().is_empty() && none.providers().is_empty());
        assert!(!none.serves(&notes));
        let stand_in = none.provider(&notes);
        assert_eq!(stand_in.kind(), &notes);
        assert!(!stand_in.describe().manage.list);
        let stored = Map::from_iter([("recall_characters".to_owned(), json!(10))]);
        assert_eq!(
            stand_in
                .check_settings(&stored)
                .map(|settings| settings.values().clone()),
            Ok(stored),
            "settings stay as stored"
        );
        let settings = Settings::default();
        assert!(stand_in.tools(&settings, MemoryWrites::Allow).is_empty(), "no tools");
        assert_eq!(
            stand_in.recall(&scope(), &Audience::Workspace, &settings).await,
            Ok(Recall::none()),
            "no recall"
        );
        let origin = Origin::Member {
            user_id: UserId::from_uuid(Uuid::from_u128(5)),
        };
        let context = CallContext {
            origin: &origin,
            audience: &Audience::Workspace,
            settings: &settings,
            writes: MemoryWrites::Allow,
            now: Timestamp::from_millis(0),
        };
        assert_eq!(
            stand_in.call(&scope(), &context, "save_memory", &Map::new()).await,
            Err(MemoryError::NotServed)
        );
        assert_eq!(
            stand_in
                .add(
                    &scope(),
                    MemoryId::from_uuid(Uuid::from_u128(6)),
                    &origin,
                    &settings,
                    "x",
                    Timestamp::from_millis(0),
                    &record_nothing,
                )
                .await,
            Err(MemoryError::NotServed)
        );
        assert_eq!(
            stand_in
                .forget_scope(&scope(), 100, &record_nothing, &never_counted)
                .await,
            Err(MemoryError::NotServed),
            "nothing to forget"
        );
        assert_eq!(
            stand_in.get(&scope(), MemoryId::from_uuid(Uuid::from_u128(6))).await,
            Err(MemoryError::NotServed),
            "nothing found by its ID"
        );
        assert_eq!(stand_in.limit(&settings), 0, "nothing kept");
        assert_eq!(
            stand_in
                .forget_all(&Forget::Organization {
                    organization_id: scope().organization_id
                })
                .await,
            Err(MemoryError::NotServed)
        );
    }

    #[tokio::test]
    async fn serves_each_kind_by_its_key() {
        let providers = MemoryProviders::none()
            .with(Arc::new(Named {
                kind: kind("notes"),
                name: "first",
            }))
            .with(Arc::new(Named {
                kind: kind("journal"),
                name: "journal",
            }))
            .with(Arc::new(Named {
                kind: kind("notes"),
                name: "second",
            }));
        assert_eq!(providers.served(), [kind("journal"), kind("notes")], "by key");
        assert_eq!(
            providers
                .descriptions()
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            ["journal", "second"],
            "a later provider of a kind replaces the earlier"
        );
        assert_eq!(providers.providers().len(), 2);
        let recall = providers
            .provider(&kind("notes"))
            .recall(&scope(), &Audience::Workspace, &Settings::default())
            .await;
        assert_eq!(
            recall.ok().and_then(|recall| recall.framed()).as_deref(),
            Some("<memory>\nsecond\n</memory>")
        );
        assert!(!providers.serves(&kind("facts")));
        assert_eq!(
            format!("{providers:?}"),
            r#"MemoryProviders { served: ["journal", "notes"] }"#
        );
        // Clones share them.
        assert!(providers.clone().serves(&kind("journal")));
    }
}
