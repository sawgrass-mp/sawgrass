-- Characters, wallets and the money ledger (DB-1).
--
-- Money is a whole number of minor units (cents) in BIGINT. A balance changes only by inserting a ledger entry,
-- which applies itself to its wallet; the entries of one operation sum to zero, and the ledger cannot be edited or
-- deleted. The triggers below enforce this for any role that does not own the tables, so a bug or a hand-written
-- query run by the server cannot create or lose money. The owner (the role that runs the migrations) can still
-- disable or drop triggers: run the server under a separate role with SELECT and INSERT, plus UPDATE (balance) on
-- wallets, which row locks need; the triggers still refuse a direct balance change from that role.

CREATE TABLE characters (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    first_name TEXT NOT NULL CHECK (char_length(first_name) BETWEEN 1 AND 32),
    last_name  TEXT NOT NULL CHECK (char_length(last_name) BETWEEN 1 AND 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A wallet belongs to a character (`cash`, `bank`) or, with no character, is a system wallet such as the treasury
-- that pays salaries and receives shop purchases. Only system wallets may go negative: money enters and leaves the
-- economy through them, so the sum of all balances is always zero.
CREATE TABLE wallets (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    character_id BIGINT REFERENCES characters (id) ON DELETE RESTRICT,
    kind         TEXT NOT NULL CHECK (char_length(kind) BETWEEN 1 AND 32),
    balance      BIGINT NOT NULL DEFAULT 0,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (character_id IS NULL OR kind IN ('cash', 'bank')),
    CHECK (character_id IS NULL OR balance >= 0)
);

CREATE UNIQUE INDEX wallets_character_kind ON wallets (character_id, kind) WHERE character_id IS NOT NULL;
CREATE UNIQUE INDEX wallets_system_kind ON wallets (kind) WHERE character_id IS NULL;

-- One row per money operation. The idempotency key comes from the caller, so a retried request is applied once.
CREATE TABLE operations (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE CHECK (char_length(idempotency_key) BETWEEN 1 AND 128),
    reason          TEXT NOT NULL CHECK (char_length(reason) BETWEEN 1 AND 64),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE ledger_entries (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    operation_id  BIGINT NOT NULL REFERENCES operations (id),
    wallet_id     BIGINT NOT NULL REFERENCES wallets (id),
    delta         BIGINT NOT NULL CHECK (delta <> 0),
    balance_after BIGINT NOT NULL,
    UNIQUE (operation_id, wallet_id)
);

CREATE INDEX ledger_entries_wallet ON ledger_entries (wallet_id, id);

-- The ledger is append-only.
CREATE FUNCTION ledger_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME USING ERRCODE = 'restrict_violation';
END
$$;

CREATE TRIGGER operations_append_only BEFORE UPDATE OR DELETE ON operations
    FOR EACH ROW EXECUTE FUNCTION ledger_append_only();
CREATE TRIGGER operations_no_truncate BEFORE TRUNCATE ON operations
    FOR EACH STATEMENT EXECUTE FUNCTION ledger_append_only();
CREATE TRIGGER ledger_entries_append_only BEFORE UPDATE OR DELETE ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION ledger_append_only();
CREATE TRIGGER ledger_entries_no_truncate BEFORE TRUNCATE ON ledger_entries
    FOR EACH STATEMENT EXECUTE FUNCTION ledger_append_only();

-- The entries of one operation sum to zero, checked at commit so that all of them can be inserted first.
CREATE FUNCTION ledger_operation_balanced() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    total NUMERIC;
BEGIN
    SELECT sum(delta) INTO total FROM ledger_entries WHERE operation_id = NEW.operation_id;
    IF total <> 0 THEN
        RAISE EXCEPTION 'operation % does not balance: entries sum to %', NEW.operation_id, total
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER ledger_entries_balanced AFTER INSERT ON ledger_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ledger_operation_balanced();

-- The ledger drives the balances. Inserting an entry locks its wallet, checks that `balance_after` is the current
-- balance plus `delta`, and applies it. That is the only way a balance changes: a direct UPDATE is refused, so an
-- entry cannot be written without moving its wallet, and a wallet cannot move without an entry.
CREATE FUNCTION ledger_entry_apply() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    current BIGINT;
BEGIN
    SELECT balance INTO current FROM wallets WHERE id = NEW.wallet_id FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'wallet % does not exist', NEW.wallet_id USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF NEW.balance_after <> current + NEW.delta THEN
        RAISE EXCEPTION 'entry for wallet % says balance_after %, but % + % is %',
            NEW.wallet_id, NEW.balance_after, current, NEW.delta, current + NEW.delta
            USING ERRCODE = 'check_violation';
    END IF;
    UPDATE wallets SET balance = NEW.balance_after WHERE id = NEW.wallet_id;
    RETURN NEW;
END
$$;

CREATE TRIGGER ledger_entries_apply BEFORE INSERT ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION ledger_entry_apply();

-- A wallet is created empty, never changes owner or kind, and changes balance only from `ledger_entry_apply`
-- (a trigger depth above 1 means the update comes from a trigger; no other trigger updates wallets).
CREATE FUNCTION wallet_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.balance <> 0 THEN
            RAISE EXCEPTION 'wallet % must be created with a zero balance', NEW.id USING ERRCODE = 'check_violation';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.id <> OLD.id OR NEW.character_id IS DISTINCT FROM OLD.character_id OR NEW.kind <> OLD.kind THEN
        RAISE EXCEPTION 'wallet % cannot change owner or kind', OLD.id USING ERRCODE = 'restrict_violation';
    END IF;
    IF NEW.balance <> OLD.balance AND pg_trigger_depth() < 2 THEN
        RAISE EXCEPTION 'wallet % balance changes only through a ledger entry', OLD.id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER wallets_guard BEFORE INSERT OR UPDATE ON wallets
    FOR EACH ROW EXECUTE FUNCTION wallet_guard();

CREATE TRIGGER wallets_no_delete BEFORE DELETE ON wallets
    FOR EACH ROW EXECUTE FUNCTION ledger_append_only();
CREATE TRIGGER wallets_no_truncate BEFORE TRUNCATE ON wallets
    FOR EACH STATEMENT EXECUTE FUNCTION ledger_append_only();
