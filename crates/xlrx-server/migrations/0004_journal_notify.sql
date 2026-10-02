-- Live updates (PLAN 5.2): every statement that writes the journal notifies listeners with the
-- highest new sequence number. Notifications are delivered on commit, in commit order.
CREATE FUNCTION journal_notify() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('xlrx_journal', (SELECT max(seq) FROM new_rows)::text);
    RETURN NULL;
END
$$;

CREATE TRIGGER journal_notify AFTER INSERT ON journal
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION journal_notify();
