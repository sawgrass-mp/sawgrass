-- The body of sawgrass_db::transfer as one server-side function: one round trip per transfer instead of five.
CREATE FUNCTION transfer_fn(key TEXT, src BIGINT, dst BIGINT, amount BIGINT, why TEXT) RETURNS BIGINT
LANGUAGE plpgsql AS $$
DECLARE
    op BIGINT; sb BIGINT; ssys BOOLEAN; db BIGINT;
BEGIN
    INSERT INTO operations (idempotency_key, reason) VALUES (key, why)
        ON CONFLICT (idempotency_key) DO NOTHING RETURNING id INTO op;
    IF op IS NULL THEN RETURN NULL; END IF;
    SELECT balance, is_system INTO sb, ssys FROM wallets_lock(ARRAY[src, dst]) WHERE id = src;
    SELECT balance INTO db FROM wallets WHERE id = dst;
    IF sb - amount < 0 AND NOT ssys THEN RAISE EXCEPTION 'insufficient funds'; END IF;
    INSERT INTO ledger_entries (operation_id, wallet_id, delta, balance_after) VALUES
        (op, src, -amount, sb - amount), (op, dst, amount, db + amount);
    RETURN op;
END $$;
