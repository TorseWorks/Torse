//! Host-owned user grants, selected only by verified transport credentials.
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use torse_net::tls::TransportSecurity;
use torse_sim::{
    action::{ActionTarget, ActorContext},
    replica::ReplicaScope,
};
use torse_types::{ContentKey, Digest, EntityId, Fault, SectionPos};

#[derive(Clone)]
pub struct UserGrant {
    pub user: ContentKey,
    pub actor: ActorContext,
    pub scope: ReplicaScope,
    pub player: Option<EntityId>,
}
impl UserGrant {
    pub fn validate(&self, sections: usize, actor_sections: usize) -> Result<(), Fault> {
        if self.actor.principal() != &self.user
            || self.scope.sections.is_empty()
            || self.scope.sections.len() > sections
            || self.actor.section_count() > actor_sections
            || self.player.is_some_and(|id| id.0 == 0)
        {
            return Err(Fault::invalid_data("invalid authenticated user grant"));
        }
        Ok(())
    }
}
/// Implementations are trusted application adapters, never a client-supplied identity.
pub trait IdentityVerifier: Send + Sync {
    fn verify(&self, credential: TransportSecurity) -> Result<UserGrant, Fault>;
    fn accounted_bytes(&self) -> u64;
}
pub struct CertificateUsers {
    users: BTreeMap<[u8; 32], UserGrant>,
}
impl CertificateUsers {
    pub fn validate_world(
        &self,
        world: &torse_sim::WorldAuthority,
        section_limit: usize,
        actor_section_limit: usize,
    ) -> Result<(), Fault> {
        for grant in self.users.values() {
            grant.validate(section_limit, actor_section_limit)?;
            for section in &grant.scope.sections {
                let origin = section
                    .to_block(
                        torse_types::LocalPos { x: 0, y: 0, z: 0 },
                        world.store().side(),
                    )
                    .map_err(|_| Fault::invalid_data("user scope coordinate overflow"))?;
                let bounds = world.store().bounds();
                let last = world.store().side() - 1;
                section
                    .to_block(
                        torse_types::LocalPos {
                            x: last,
                            y: last,
                            z: last,
                        },
                        world.store().side(),
                    )
                    .map_err(|_| Fault::invalid_data("user scope coordinate overflow"))?;
                let end = origin
                    .y
                    .checked_add(i64::from(world.store().side()))
                    .ok_or_else(|| Fault::invalid_data("user scope coordinate overflow"))?;
                if origin.y >= bounds.max_y || end <= bounds.min_y {
                    return Err(Fault::invalid_data("user scope outside world bounds"));
                }
            }
            if let Some(player) = grant.player {
                let entity = world
                    .entities()
                    .handle_of(player)
                    .and_then(|handle| world.entities().get(handle))
                    .ok_or_else(|| Fault::invalid_data("user player is not resident"))?;
                if !entity
                    .position
                    .section(world.store().side())
                    .is_some_and(|section| grant.scope.sections.contains(&section))
                    || torse_world::PlayerMotion::of(entity)
                        .ok()
                        .flatten()
                        .is_none()
                    || torse_world::EntityCollider::of(entity)
                        .ok()
                        .flatten()
                        .and_then(|collider| collider.half())
                        .is_none()
                {
                    return Err(Fault::invalid_data(
                        "user player is outside scope or lacks movement state",
                    ));
                }
            }
        }
        Ok(())
    }
    /// Conservative defaults for small static policy files. Larger trusted
    /// registries must choose explicit limits with `new_with_limits`.
    pub fn new(users: impl IntoIterator<Item = (Digest, UserGrant)>) -> Result<Self, Fault> {
        Self::new_with_limits(
            users,
            64,
            crate::bootstrap::execution_limits().actor_sections,
        )
    }
    pub fn new_with_limits(
        users: impl IntoIterator<Item = (Digest, UserGrant)>,
        section_limit: usize,
        actor_section_limit: usize,
    ) -> Result<Self, Fault> {
        if section_limit == 0 || actor_section_limit == 0 {
            return Err(Fault::invalid_data("user grant limits must be nonzero"));
        }
        let mut registry = BTreeMap::new();
        let mut identities = BTreeSet::new();
        let mut players = BTreeSet::new();
        for (certificate, grant) in users {
            grant.validate(section_limit, actor_section_limit)?;
            if registry.len() >= 64
                || !identities.insert(grant.user.clone())
                || grant.player.is_some_and(|player| !players.insert(player))
                || registry.insert(certificate.0, grant).is_some()
            {
                return Err(Fault::invalid_data(
                    "duplicate or excessive authenticated users",
                ));
            }
        }
        if registry.is_empty() {
            return Err(Fault::invalid_data("empty authenticated user registry"));
        }
        Ok(Self { users: registry })
    }
    /// Bounded local configuration. It contains certificate fingerprints, never private keys.
    pub fn load(path: &Path) -> Result<Self, Fault> {
        Self::load_with_limits(
            path,
            64,
            crate::bootstrap::execution_limits().actor_sections,
        )
    }
    /// Scope limits are host policy; the file still retains its own 64 KiB
    /// parser bound and bounded user/permission/entity collections.
    pub fn load_with_limits(
        path: &Path,
        section_limit: usize,
        actor_section_limit: usize,
    ) -> Result<Self, Fault> {
        use std::io::Read;
        let file = std::fs::File::open(path)
            .map_err(|_| Fault::invalid_data("cannot open user policy"))?;
        let mut bytes = Vec::new();
        file.take(65_537)
            .read_to_end(&mut bytes)
            .map_err(|_| Fault::invalid_data("cannot read user policy"))?;
        if bytes.len() > 65_536 {
            return Err(Fault::resource_limit("user policy exceeds 64 KiB"));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| Fault::invalid_data("user policy is not UTF-8"))?;
        let document: Document =
            toml::from_str(text).map_err(|_| Fault::invalid_data("invalid user policy"))?;
        if document.users.len() > 64 {
            return Err(Fault::resource_limit("user policy count"));
        }
        Self::new_with_limits(
            document
                .users
                .into_iter()
                .map(|user| user.grant(section_limit.min(actor_section_limit)))
                .collect::<Result<Vec<_>, _>>()?,
            section_limit,
            actor_section_limit,
        )
    }
}
impl IdentityVerifier for CertificateUsers {
    fn verify(&self, credential: TransportSecurity) -> Result<UserGrant, Fault> {
        let TransportSecurity::MutualTls(digest) = credential else {
            return Err(Fault::incompatible("verified mutual TLS is required"));
        };
        self.users
            .get(&digest.0)
            .cloned()
            .ok_or_else(|| Fault::incompatible("client certificate has no user grant"))
    }
    fn accounted_bytes(&self) -> u64 {
        self.users.values().fold(4096u64, |bytes, grant| {
            bytes
                .saturating_add(grant.actor.accounted_bytes().saturating_mul(2))
                .saturating_add(grant.scope.sections.len() as u64 * 8192)
                .saturating_add(8192)
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    users: Vec<User>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    id: String,
    certificate_sha256: String,
    sections: Vec<[i64; 3]>,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    entities: Vec<u128>,
    player: Option<u128>,
}
impl User {
    fn grant(self, section_limit: usize) -> Result<(Digest, UserGrant), Fault> {
        if self.sections.len() > section_limit
            || self.permissions.len() > 32
            || self.entities.len() > 64
        {
            return Err(Fault::resource_limit("user grant collection limit"));
        }
        let parse = |value: &str| {
            ContentKey::parse(value)
                .map_err(|_| Fault::invalid_data("invalid user policy content key"))
        };
        let user = parse(&self.id)?;
        let mut actor = ActorContext::new(user.clone());
        for permission in self.permissions {
            actor = actor.permission(parse(&permission)?);
        }
        for entity in self.entities {
            if entity == 0 {
                return Err(Fault::invalid_data("invalid user entity grant"));
            }
            actor = actor.target(ActionTarget::Entity(EntityId(entity)));
        }
        let scope = ReplicaScope {
            sections: self
                .sections
                .into_iter()
                .map(|[x, y, z]| SectionPos::new(x, y, z))
                .collect(),
        };
        for section in &scope.sections {
            actor = actor.section(*section);
        }
        let bytes = self.certificate_sha256.as_bytes();
        if bytes.len() != 64 {
            return Err(Fault::invalid_data(
                "certificate SHA-256 must contain 64 hex digits",
            ));
        }
        let mut fingerprint = [0u8; 32];
        let hex = |byte: u8| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(Fault::invalid_data("invalid certificate fingerprint")),
        };
        for (index, pair) in bytes.chunks_exact(2).enumerate() {
            fingerprint[index] = hex(pair[0])? * 16 + hex(pair[1])?;
        }
        Ok((
            Digest(fingerprint),
            UserGrant {
                user,
                actor,
                scope,
                player: self.player.map(EntityId),
            },
        ))
    }
}
