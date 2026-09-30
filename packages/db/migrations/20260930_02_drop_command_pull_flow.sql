-- Remove the dead command pull-flow.
--
-- `commands` and `command_receipts` were a second, parallel control plane.
-- Nothing ever executed from them: the command engine, actuators, dashboard,
-- fabric status, metrics and rules engine all use `command_queue`. So
-- POST /api/commands/enqueue wrote a row that would never run and returned
-- 200 — a silent black hole for anyone who called it.
--
-- They were also reachable without proving identity. /pull took the target
-- node from a query string and flipped those rows to 'sent', letting any
-- authenticated caller drain and blackhole another outpost's queue; /ack took
-- the node from the request body and wrote receipts in its name. Deleting the
-- path removes three confused-deputy holes rather than patching them.
--
-- The push flow (`command_queue` + POST /api/commands/execute) is unaffected.

DROP TABLE IF EXISTS command_receipts;
DROP TABLE IF EXISTS commands;
