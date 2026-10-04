-- Failed sign-in attempts, so a slow guess is stopped too.
--
-- The in-process token bucket stops a flood. It does not stop patience: six
-- attempts a minute, forever, from a rotating set of addresses, is inside every
-- rate limit and will eventually find a weak password. That attack is only
-- visible if the failures are *remembered*, and remembered across restarts —
-- an attacker who can make the process restart should not get a fresh
-- allowance.
--
-- Two counters, because the two attacks have opposite shapes:
--
--   per account  one password, many guesses. Caught by counting failures for
--                an email regardless of where they came from.
--   per address  many accounts, one guess each (credential stuffing, using a
--                leaked password list). Caught by counting failures from an
--                address regardless of which account they targeted.
--
-- The per-account counter has a cost worth naming: it lets somebody lock a
-- known email out by failing against it deliberately. That is why the
-- thresholds differ — the per-address window is tight and short, the
-- per-account one is looser and longer, so normal mistyping never trips it and
-- a targeted lockout is slow, noisy and self-limiting. Perfect answers to this
-- need a second factor, which does not exist here yet.

CREATE TABLE IF NOT EXISTS auth_attempts (
    id          BIGSERIAL PRIMARY KEY,

    -- Lowercased, matching how login looks an account up. Stored even when no
    -- such account exists: refusing to count attempts against unknown emails
    -- would let an attacker enumerate accounts by watching which addresses
    -- start getting throttled.
    email       TEXT NOT NULL,

    -- Text rather than INET. The only query against it is equality, so the
    -- network type would buy nothing and would mean enabling another sqlx
    -- feature and another type mapping to get wrong. Null when the request
    -- arrived with no usable address; those share one bucket rather than
    -- escaping the count.
    ip          TEXT,

    succeeded   BOOLEAN NOT NULL,
    at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The two hot reads: recent failures for an account, and recent failures from
-- an address. Partial on failure because successes are never counted toward a
-- lockout and would otherwise bloat both indexes.
CREATE INDEX IF NOT EXISTS idx_auth_attempts_email
    ON auth_attempts (email, at DESC) WHERE NOT succeeded;
CREATE INDEX IF NOT EXISTS idx_auth_attempts_ip
    ON auth_attempts (ip, at DESC) WHERE NOT succeeded AND ip IS NOT NULL;

-- Pruning needs this, and without it the table only ever grows: a login table
-- is append-only on the busiest path in the application.
CREATE INDEX IF NOT EXISTS idx_auth_attempts_at ON auth_attempts (at);

COMMENT ON TABLE auth_attempts IS
  'Failed sign-ins, remembered across restarts. Two counters: per account, and per address.';
COMMENT ON COLUMN auth_attempts.email IS
  'Recorded even for unknown accounts, so throttling cannot be used to enumerate them.';
