-- Characters, wallets and the money ledger (DB-1).
--
-- Money is a whole number of minor units (cents) in BIGINT. A balance never changes on its own: every change is a
-- ledger entry written in the same transaction as the wallet update, the entries of one operation sum to zero,
-- and the ledger cannot be edited or deleted. The triggers below enforce this in the database, so a bug or a
-- hand-written query cannot create or lose money without failing.

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

-- A wallet is created empty, and its balance changes only to the `balance_after` of a new ledger entry for it.
-- Rows are locked before the entry is written, so the newest entry of a wallet is the one this transaction added.
CREATE FUNCTION wallet_balance_from_ledger() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    latest ledger_entries%ROWTYPE;
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
    IF NEW.balance <> OLD.balance THEN
        SELECT * INTO latest FROM ledger_entries WHERE wallet_id = NEW.id ORDER BY id DESC LIMIT 1;
        IF NOT FOUND OR latest.balance_after <> NEW.balance OR latest.delta <> NEW.balance - OLD.balance THEN
            RAISE EXCEPTION 'wallet % balance changed without a matching ledger entry', NEW.id
                USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER wallets_balance_from_ledger BEFORE INSERT OR UPDATE ON wallets
    FOR EACH ROW EXECUTE FUNCTION wallet_balance_from_ledger();

CREATE TRIGGER wallets_no_delete BEFORE DELETE ON wallets
    FOR EACH ROW EXECUTE FUNCTION ledger_append_only();
CREATE TRIGGER wallets_no_truncate BEFORE TRUNCATE ON wallets
    FOR EACH STATEMENT EXECUTE FUNCTION ledger_append_only();
