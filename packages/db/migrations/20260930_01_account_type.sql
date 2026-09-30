-- Marketplace account type: what a user came here to do.
--
-- Deliberately separate from `users.role`. `role` is the *security* role
-- (user | admin) and decides whether a token reaches AdminUser routes; it must
-- never be self-assignable at signup. `account_type` is a profile attribute
-- that shapes onboarding and nothing else:
--
--   buyer  - posts orders, receives goods
--   seller - bids on orders, ships goods, must have a payout wallet before a
--            settlement naming them can be recorded (see settlement.rs)
--   both   - does both, which most outposts in a fabric actually do
--
-- It is not a permission. The marketplace already enforces what matters
-- structurally: only the requester accepts a bid, and submit_bid refuses a bid
-- on your own order. Recording the intent lets onboarding ask a seller for a
-- wallet up front instead of failing at settlement time.

ALTER TABLE users
  ADD COLUMN IF NOT EXISTS account_type TEXT NOT NULL DEFAULT 'buyer';

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'users_account_type_check'
  ) THEN
    ALTER TABLE users
      ADD CONSTRAINT users_account_type_check
      CHECK (account_type IN ('buyer', 'seller', 'both'));
  END IF;
END $$;

-- Existing users predate the column. Anyone who has already bid is evidently a
-- seller; anyone who has only ordered stays the 'buyer' default.
UPDATE users u
   SET account_type = 'both'
 WHERE EXISTS (SELECT 1 FROM bids   b WHERE b.bidder_id    = u.id)
   AND EXISTS (SELECT 1 FROM orders o WHERE o.requester_id = u.id);

UPDATE users u
   SET account_type = 'seller'
 WHERE account_type = 'buyer'
   AND EXISTS (SELECT 1 FROM bids b WHERE b.bidder_id = u.id);

-- Signup looks users up by email; the unique index already exists on users.email.
