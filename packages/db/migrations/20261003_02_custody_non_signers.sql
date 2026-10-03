-- Let the custody chain record a handover to a party that cannot sign.
--
-- The chain was built for the fabric, where every party is a node with a key:
-- `to_node_id` is NOT NULL, and the only valid subjects are an order or a
-- fulfilment. That is exactly right for outpost-to-outpost custody and it
-- cannot express the handover that matters most on Earth — giving a parcel to
-- UPS.
--
-- Three things had to give, and each is a generalisation rather than a
-- loosening:
--
--   1. A recipient must be *identified*, not necessarily *keyed*. A commercial
--      carrier has a name and a tracking number and no Ed25519 key, and that
--      is a real recipient.
--   2. A receipt's subject can be a carrier shipment. An organisation posting
--      its own stock to its own customer has no marketplace order, and
--      refusing it a custody chain would mean the single-org case — the one
--      that makes this tool useful before a marketplace exists — has no
--      provenance at all.
--   3. Nothing about `verified` changes. A carrier cannot sign, so its
--      receipts stay unverified, and the distinction between "somebody signed
--      for this" and "a third party reported it" stays visible. That
--      distinction is the whole value of the chain and is not negotiable.

-- === 1. A recipient can be named rather than keyed ===
ALTER TABLE custody_receipts ALTER COLUMN to_node_id DROP NOT NULL;

-- But it must still be *somebody*. A receipt with neither a node nor a label
-- records that custody changed hands to nobody in particular, which is worse
-- than no receipt: it looks like provenance and carries none.
ALTER TABLE custody_receipts DROP CONSTRAINT IF EXISTS custody_recipient_identified;
ALTER TABLE custody_receipts
  ADD CONSTRAINT custody_recipient_identified
  CHECK (to_node_id IS NOT NULL OR to_label IS NOT NULL);

-- A signature without a node is meaningless — there is no key to check it
-- against. Catching it here stops a receipt claiming to be signed by a party
-- that cannot have signed.
ALTER TABLE custody_receipts DROP CONSTRAINT IF EXISTS custody_signature_needs_a_key;
ALTER TABLE custody_receipts
  ADD CONSTRAINT custody_signature_needs_a_key
  CHECK (signature IS NULL OR to_node_id IS NOT NULL);

-- === 2. A carrier shipment can be the subject ===
ALTER TABLE custody_receipts ADD COLUMN IF NOT EXISTS carrier_shipment_id UUID
  REFERENCES carrier_shipments(id) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS idx_custody_carrier_shipment
  ON custody_receipts (carrier_shipment_id) WHERE carrier_shipment_id IS NOT NULL;

ALTER TABLE custody_receipts DROP CONSTRAINT IF EXISTS custody_has_subject;
ALTER TABLE custody_receipts
  ADD CONSTRAINT custody_has_subject
  CHECK (order_id IS NOT NULL OR fulfillment_id IS NOT NULL OR carrier_shipment_id IS NOT NULL);

-- === 3. The event vocabulary stays as it was ===
--
-- Deliberately not extended. A handover to a carrier is a `transfer` and an
-- arrival is a `delivery` — the existing words describe it accurately, and
-- adding 'handover' as a synonym would mean two spellings of one event and
-- every future reader having to know both.
COMMENT ON COLUMN custody_receipts.to_node_id IS
  'Null when the recipient has no key — a commercial carrier. to_label names them instead.';
COMMENT ON COLUMN custody_receipts.carrier_shipment_id IS
  'Subject for a leg with no marketplace order: the single-org shipping case.';
COMMENT ON CONSTRAINT custody_signature_needs_a_key ON custody_receipts IS
  'A signature with no node id cannot be checked against anything.';
