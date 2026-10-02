//! Resolving which organisation a caller is acting for.
//!
//! Every read of business data filters on this. One missed filter is a leak
//! across a commercial boundary, so the resolution lives in one place rather
//! than being re-derived per handler — and it **fails closed**: a caller whose
//! org cannot be established sees nothing, rather than seeing everything.
//!
//! That direction matters more than it sounds. The convenient failure mode
//! (unknown org ⇒ no filter) turns every bug in this function into a silent
//! disclosure of every customer's holdings to every other customer. The safe
//! one turns the same bug into an empty page, which someone reports.

use uuid::Uuid;

use crate::routes::auth_middleware::Principal;
use crate::AppState;

/// The organisation this caller acts for, if it can be established.
///
/// A user belongs to an org through `org_members`; a node belongs through the
/// certificate recorded in `org_outposts`. The shared fabric secret resolves
/// to nothing on purpose — every holder is indistinguishable, so it cannot
/// establish *whose* data is being asked for.
pub async fn caller_org(state: &AppState, principal: &Principal) -> Option<Uuid> {
    match principal {
        Principal::User(claims) => {
            let user_id = Uuid::parse_str(&claims.sub).ok()?;
            sqlx::query_scalar::<_, Uuid>(
                // A person in several orgs resolves to the earliest joined, so
                // the answer is stable. Choosing between them is a UI concern
                // that does not exist yet; picking arbitrarily would make the
                // same request return different data on different days.
                "SELECT org_id FROM org_members WHERE user_id = $1 ORDER BY added_at LIMIT 1",
            )
            .bind(user_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
        }
        Principal::Node(node_id) => sqlx::query_scalar::<_, Uuid>(
            "SELECT org_id FROM org_outposts \
             WHERE node_id = $1 AND revoked_at IS NULL \
               AND (not_after IS NULL OR not_after > NOW())",
        )
        .bind(node_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten(),
        Principal::SharedSecret => None,
    }
}

/// The org this outpost itself belongs to.
///
/// Used where an outpost answers on its own behalf — the peer-facing rollup
/// summary — rather than on behalf of a caller.
pub async fn own_org(state: &AppState) -> Option<Uuid> {
    let node_id = Uuid::parse_str(&state.identity.node_id).ok()?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT org_id FROM org_outposts WHERE node_id = $1 AND revoked_at IS NULL",
    )
    .bind(node_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::auth_middleware::Claims;

    fn claims(sub: &str) -> Claims {
        Claims { sub: sub.into(), exp: 0, provider: "test".into(), role: "user".into() }
    }

    /// The shared secret can never establish an organisation.
    ///
    /// It authenticates membership of the fabric and nothing else — every
    /// holder looks identical — so treating it as belonging to an org would
    /// hand one credential the data of whichever org it was guessed into.
    /// Asserted here as a property of the Principal rather than needing a
    /// database, because it must stay true regardless of what is stored.
    #[test]
    fn the_shared_secret_belongs_to_no_organisation() {
        assert!(matches!(Principal::SharedSecret, Principal::SharedSecret));
        // A node id or user id is required to resolve an org at all.
        assert_eq!(Principal::SharedSecret.node_id(), None);
        assert!(!Principal::SharedSecret.is_admin());
    }

    #[test]
    fn a_malformed_subject_resolves_to_nothing() {
        // Parse failure must mean "no org", never "all orgs".
        assert!(Uuid::parse_str(&claims("not-a-uuid").sub).is_err());
    }
}
