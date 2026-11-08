//! Which commit identities the Git surface offers and which one a repository
//! acts as (`views/git/gitAuthors.ts`, `lib/source-control/identity.ts`).

/// The System identity's ID: the machine's own author, which a repository
/// uses when it names none of its own.
pub const SYSTEM_IDENTITY_ID: &str = "global";

/// How an identity authenticates transfers (`identityTransport`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityTransport {
    System,
    Account,
    Ssh,
    Anonymous,
}

/// A stored identity, or the System one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub user_name: String,
    pub user_email: String,
    pub transport: IdentityTransport,
    /// The connected account an account identity acts as.
    pub has_account: bool,
    pub has_ssh_key: bool,
}

impl Identity {
    /// The System identity from the machine's author, which may be empty.
    pub fn system(user_name: Option<String>, user_email: Option<String>) -> Self {
        let user_name = user_name.unwrap_or_default();
        Self {
            id: SYSTEM_IDENTITY_ID.to_owned(),
            name: user_name.clone(),
            user_name,
            user_email: user_email.unwrap_or_default(),
            transport: IdentityTransport::System,
            has_account: false,
            has_ssh_key: false,
        }
    }

    pub fn is_system(&self) -> bool {
        self.id == SYSTEM_IDENTITY_ID
    }

    /// `isCompleteIdentity`: an author, and a way to authenticate.
    pub fn is_complete(&self) -> bool {
        !self.user_name.is_empty()
            && !self.user_email.is_empty()
            && match self.transport {
                IdentityTransport::Account => self.has_account,
                IdentityTransport::Ssh => self.has_ssh_key,
                IdentityTransport::System | IdentityTransport::Anonymous => true,
            }
    }

    fn signs_as(&self, user_name: &str, user_email: &str) -> bool {
        self.user_name == user_name && self.user_email == user_email
    }
}

/// `offeredAuthors`: the System identity first, then each complete stored
/// one. Which accounts are connected is not known here, which the web reads
/// as every account being usable.
pub fn offered(profiles: &[Identity], system: Option<&Identity>) -> Vec<Identity> {
    let mut offered: Vec<Identity> = system.cloned().into_iter().collect();
    for profile in profiles {
        if profile.is_system() || !profile.is_complete() {
            continue;
        }
        match offered.iter_mut().find(|known| known.id == profile.id) {
            Some(known) => *known = profile.clone(),
            None => offered.push(profile.clone()),
        }
    }
    offered
}

/// `activeAuthor` without a binding: a repository naming no complete author
/// is on the System identity; else a stored identity signing as that author,
/// else the System one when it matches, else the author shown as itself.
pub fn active(
    profiles: &[Identity],
    system: Option<&Identity>,
    author: (Option<&str>, Option<&str>),
) -> Option<Identity> {
    let (Some(user_name), Some(user_email)) = author else {
        return system.cloned();
    };
    if user_name.is_empty() || user_email.is_empty() {
        return system.cloned();
    }
    if let Some(stored) = profiles
        .iter()
        .find(|profile| profile.signs_as(user_name, user_email))
    {
        return Some(stored.clone());
    }
    if let Some(system) = system.filter(|system| system.signs_as(user_name, user_email)) {
        return Some(system.clone());
    }
    Some(Identity {
        id: "local-config".to_owned(),
        name: user_name.to_owned(),
        user_name: user_name.to_owned(),
        user_email: user_email.to_owned(),
        transport: IdentityTransport::System,
        has_account: false,
        has_ssh_key: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(id: &str, email: &str, transport: IdentityTransport) -> Identity {
        Identity {
            id: id.to_owned(),
            name: id.to_owned(),
            user_name: "Ada".to_owned(),
            user_email: email.to_owned(),
            transport,
            has_account: false,
            has_ssh_key: false,
        }
    }

    #[test]
    fn the_system_identity_comes_first_and_incomplete_ones_stay_out() {
        let system = Identity::system(Some("Ada".into()), Some("ada@home".into()));
        let profiles = [
            stored("work", "ada@work", IdentityTransport::System),
            stored("key", "ada@key", IdentityTransport::Ssh),
            Identity {
                user_email: String::new(),
                ..stored("blank", "", IdentityTransport::Anonymous)
            },
        ];
        let ids: Vec<String> = offered(&profiles, Some(&system))
            .into_iter()
            .map(|identity| identity.id)
            .collect();
        assert_eq!(ids, ["global", "work"], "an SSH identity needs its key");
        let empty_system = Identity::system(None, None);
        assert_eq!(
            offered(&[], Some(&empty_system)).len(),
            1,
            "a machine without an author still offers System"
        );
    }

    #[test]
    fn the_author_picks_the_active_identity() {
        let system = Identity::system(Some("Ada".into()), Some("ada@home".into()));
        let profiles = [stored("work", "ada@work", IdentityTransport::System)];
        assert_eq!(
            active(&profiles, Some(&system), (None, None)).map(|i| i.id),
            Some("global".to_owned())
        );
        assert_eq!(
            active(&profiles, Some(&system), (Some("Ada"), Some("ada@work"))).map(|i| i.id),
            Some("work".to_owned())
        );
        assert_eq!(
            active(&profiles, Some(&system), (Some("Ada"), Some("ada@home"))).map(|i| i.id),
            Some("global".to_owned())
        );
        let local = active(&profiles, Some(&system), (Some("Bo"), Some("bo@x"))).expect("local");
        assert_eq!(
            (local.id.as_str(), local.name.as_str()),
            ("local-config", "Bo")
        );
    }
}
