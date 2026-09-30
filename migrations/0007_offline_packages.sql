ALTER TABLE users
    ADD COLUMN enable_content_downloading BOOLEAN NOT NULL DEFAULT TRUE;

CREATE TABLE offline_user_quotas (
    user_id UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    quota_bytes BIGINT NOT NULL DEFAULT 21474836480 CHECK (quota_bytes BETWEEN 0 AND 1099511627776),
    reserved_bytes BIGINT NOT NULL DEFAULT 0 CHECK (reserved_bytes >= 0),
    used_bytes BIGINT NOT NULL DEFAULT 0 CHECK (used_bytes >= 0),
    cleanup_bytes BIGINT NOT NULL DEFAULT 0 CHECK (cleanup_bytes >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (reserved_bytes + used_bytes + cleanup_bytes <= quota_bytes)
);

CREATE TABLE offline_packages (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id UUID NOT NULL,
    library_id UUID NOT NULL,
    item_name TEXT NOT NULL CHECK (length(item_name) BETWEEN 1 AND 512),
    item_type TEXT NOT NULL CHECK (length(item_type) BETWEEN 1 AND 64),
    file_name TEXT NOT NULL DEFAULT 'offline-media' CHECK (
        length(file_name) BETWEEN 1 AND 240
        AND position('/' in file_name) = 0
        AND position(chr(92) in file_name) = 0
        AND file_name !~ '[[:cntrl:]]'
    ),
    container TEXT CHECK (container IS NULL OR length(container) <= 32),
    source_size BIGINT NOT NULL CHECK (source_size BETWEEN 1 AND 8589934592),
    source_modified_at TIMESTAMPTZ NOT NULL,
    reservation_bytes BIGINT NOT NULL CHECK (reservation_bytes >= 0),
    bytes_copied BIGINT NOT NULL DEFAULT 0 CHECK (bytes_copied >= 0),
    actual_size BIGINT CHECK (actual_size IS NULL OR actual_size BETWEEN 1 AND 8589934592),
    sha256 CHAR(64) CHECK (sha256 IS NULL OR sha256 ~ '^[0-9a-f]{64}$'),
    relative_path TEXT CHECK (relative_path IS NULL OR relative_path ~ '^[0-9a-f-]{36}/[0-9a-f-]{36}[.](media|partial)$'),
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'ready', 'failed', 'cancelled')),
    claimed_run_id UUID,
    error_code TEXT CHECK (error_code IS NULL OR error_code IN (
        'source-changed', 'source-unavailable', 'policy-revoked', 'copy-failed',
        'checksum-failed', 'quota-exceeded', 'server-restarted', 'cancelled'
    )),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (source_size = reservation_bytes OR status IN ('ready', 'failed', 'cancelled')),
    CHECK ((status = 'running') = (claimed_run_id IS NOT NULL)),
    CHECK (status NOT IN ('queued', 'running') OR reservation_bytes > 0),
    CHECK (status <> 'ready' OR (actual_size = source_size AND sha256 IS NOT NULL AND relative_path IS NOT NULL AND reservation_bytes = 0)),
    CHECK ((status IN ('ready', 'failed', 'cancelled')) = (finished_at IS NOT NULL)),
    CHECK (bytes_copied <= source_size)
);

CREATE UNIQUE INDEX offline_packages_active_item_idx
    ON offline_packages (user_id, item_id)
    WHERE status IN ('queued', 'running', 'ready');
CREATE INDEX offline_packages_queue_idx
    ON offline_packages (created_at, id)
    WHERE status = 'queued';
CREATE INDEX offline_packages_user_history_idx
    ON offline_packages (user_id, created_at DESC, id);
CREATE INDEX offline_packages_claim_idx
    ON offline_packages (claimed_run_id, id)
    WHERE status = 'running';

CREATE TABLE offline_package_chunks (
    package_id UUID NOT NULL REFERENCES offline_packages(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
    byte_length INTEGER NOT NULL CHECK (byte_length BETWEEN 1 AND 1048576),
    sha256 CHAR(64) NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    PRIMARY KEY (package_id, chunk_index)
);

CREATE TABLE offline_global_quota (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    quota_bytes BIGINT NOT NULL DEFAULT 1099511627776 CHECK (quota_bytes BETWEEN 0 AND 1099511627776),
    reserved_bytes BIGINT NOT NULL DEFAULT 0 CHECK (reserved_bytes >= 0),
    used_bytes BIGINT NOT NULL DEFAULT 0 CHECK (used_bytes >= 0),
    cleanup_bytes BIGINT NOT NULL DEFAULT 0 CHECK (cleanup_bytes >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (reserved_bytes + used_bytes + cleanup_bytes <= quota_bytes)
);
INSERT INTO offline_global_quota(singleton)
VALUES (TRUE)
ON CONFLICT (singleton) DO NOTHING;

-- Cleanup work is durable and keyset-like: every pass claims the oldest
-- queued path and deletes it before moving on, so early filesystem entries
-- cannot starve later orphan files.
CREATE TABLE offline_orphan_files (
    relative_path TEXT PRIMARY KEY CHECK (relative_path ~ '^[0-9a-f-]{36}/[0-9a-f-]{36}[.](media|partial)$'),
    package_id UUID NOT NULL,
    user_id UUID,
    byte_size BIGINT NOT NULL DEFAULT 0 CHECK (byte_size >= 0),
    charge_accounted BOOLEAN NOT NULL DEFAULT TRUE,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_attempt_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX offline_orphan_files_retry_idx
    ON offline_orphan_files (last_attempt_at NULLS FIRST, created_at, relative_path);

CREATE FUNCTION enqueue_offline_file(p_relative_path TEXT, p_package_id UUID, p_user_id UUID, p_bytes BIGINT, p_charge BOOLEAN) RETURNS VOID
LANGUAGE plpgsql AS $$
BEGIN
    IF p_relative_path IS NULL THEN
        RETURN;
    END IF;
    INSERT INTO offline_orphan_files(relative_path,package_id,user_id,byte_size,charge_accounted)
    VALUES (p_relative_path,p_package_id,p_user_id,GREATEST(0,COALESCE(p_bytes,0)),p_charge)
    ON CONFLICT (relative_path) DO UPDATE SET
        byte_size=GREATEST(offline_orphan_files.byte_size,EXCLUDED.byte_size),
        charge_accounted=offline_orphan_files.charge_accounted OR EXCLUDED.charge_accounted;
END;
$$;

-- Application transactions take locks in this order: active-run marker, user
-- or source item, global quota, package row, then per-user quota. User/item
-- cascades pre-lock the global quota and all affected package rows in a stable
-- order before invoking this trigger. Direct ad-hoc package SQL is maintenance
-- only; concurrent writers must use the application lock order.
CREATE FUNCTION account_offline_package_change() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    old_reserved BIGINT := 0;
    old_used BIGINT := 0;
    old_cleanup BIGINT := 0;
    became_terminal BOOLEAN := FALSE;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        became_terminal := OLD.status IN ('queued', 'running', 'ready') AND NEW.status IN ('failed', 'cancelled');
        IF became_terminal THEN
            DELETE FROM offline_package_chunks WHERE package_id = OLD.id;
        END IF;
    END IF;
    IF TG_OP = 'DELETE' THEN
        became_terminal := OLD.status IN ('queued', 'running', 'ready');
        IF OLD.status IN ('queued', 'running') THEN old_reserved := OLD.reservation_bytes; END IF;
        IF OLD.status = 'ready' THEN old_used := COALESCE(OLD.actual_size, 0); END IF;
        IF OLD.relative_path IS NOT NULL AND OLD.status = 'queued' THEN old_cleanup := OLD.source_size; END IF;
        IF OLD.relative_path IS NOT NULL AND OLD.status = 'running' THEN old_cleanup := OLD.source_size; END IF;
        IF OLD.relative_path IS NOT NULL AND OLD.status = 'ready' THEN old_cleanup := COALESCE(OLD.actual_size, 0); END IF;
        PERFORM enqueue_offline_file(OLD.relative_path,OLD.id,OLD.user_id,old_cleanup,TRUE);
    ELSE
        became_terminal := OLD.status IN ('queued', 'running', 'ready') AND NEW.status IN ('failed', 'cancelled');
        IF became_terminal AND OLD.status IN ('queued', 'running') THEN old_reserved := OLD.reservation_bytes; END IF;
        IF became_terminal AND OLD.status = 'ready' THEN old_used := COALESCE(OLD.actual_size, 0); END IF;
        IF became_terminal AND OLD.relative_path IS NOT NULL AND OLD.status = 'queued' THEN old_cleanup := OLD.source_size; END IF;
        IF OLD.relative_path IS NOT NULL
           AND (NEW.relative_path IS DISTINCT FROM OLD.relative_path OR (OLD.status = 'running' AND NEW.status = 'queued'))
           AND NOT (OLD.status = 'running' AND NEW.status = 'ready') THEN
            IF OLD.status = 'running' AND NEW.status = 'queued' THEN
                old_cleanup := OLD.source_size;
                PERFORM enqueue_offline_file(OLD.relative_path,OLD.id,OLD.user_id,old_cleanup,FALSE);
            ELSE
                IF OLD.status = 'running' THEN old_cleanup := OLD.source_size; END IF;
                IF OLD.status = 'ready' THEN old_cleanup := COALESCE(OLD.actual_size, 0); END IF;
                IF OLD.status = 'queued' AND became_terminal THEN
                    old_cleanup := OLD.source_size;
                    PERFORM enqueue_offline_file(OLD.relative_path,OLD.id,OLD.user_id,old_cleanup,TRUE);
                ELSIF OLD.status <> 'queued' THEN
                    PERFORM enqueue_offline_file(OLD.relative_path,OLD.id,OLD.user_id,old_cleanup,TRUE);
                END IF;
            END IF;
        END IF;
    END IF;
    IF old_reserved > 0 OR old_used > 0 OR (became_terminal AND old_cleanup > 0) THEN
        -- Application mutations pre-lock this singleton before taking package
        -- locks. Direct SQL transitions still maintain accounting, but callers
        -- must serialize them when concurrent package writes are possible.
        PERFORM singleton FROM offline_global_quota WHERE singleton = TRUE FOR UPDATE;
        UPDATE offline_user_quotas
           SET reserved_bytes = GREATEST(0, reserved_bytes - old_reserved),
               used_bytes = GREATEST(0, used_bytes - old_used),
               cleanup_bytes = cleanup_bytes + CASE WHEN became_terminal THEN old_cleanup ELSE 0 END,
               updated_at = NOW()
         WHERE user_id = OLD.user_id;
        UPDATE offline_global_quota
           SET reserved_bytes = GREATEST(0, reserved_bytes - old_reserved),
               used_bytes = GREATEST(0, used_bytes - old_used),
               cleanup_bytes = cleanup_bytes + CASE WHEN became_terminal THEN old_cleanup ELSE 0 END,
               updated_at = NOW()
         WHERE singleton = TRUE;
    END IF;
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER offline_package_account_before_update
    BEFORE UPDATE ON offline_packages
    FOR EACH ROW EXECUTE FUNCTION account_offline_package_change();
CREATE TRIGGER offline_package_account_before_delete
    BEFORE DELETE ON offline_packages
    FOR EACH ROW EXECUTE FUNCTION account_offline_package_change();

-- Pre-lock all affected rows in application order before the user quota
-- cascade: global quota, packages ordered by item/id, then per-user quotas.
CREATE FUNCTION delete_offline_packages_for_user() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM singleton FROM offline_global_quota WHERE singleton = TRUE FOR UPDATE;
    PERFORM id FROM offline_packages WHERE user_id = OLD.id ORDER BY item_id, id FOR UPDATE;
    DELETE FROM offline_packages WHERE user_id = OLD.id;
    RETURN OLD;
END;
$$;
CREATE TRIGGER users_delete_offline_packages_before_delete
    BEFORE DELETE ON users
    FOR EACH ROW EXECUTE FUNCTION delete_offline_packages_for_user();

-- Revoke server copies when the source catalog item is deliberately removed.
-- The package trigger handles quota accounting and durable file cleanup.
CREATE FUNCTION cancel_offline_packages_for_item_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    package_row RECORD;
BEGIN
    PERFORM singleton FROM offline_global_quota WHERE singleton = TRUE FOR UPDATE;
    PERFORM id FROM offline_packages
     WHERE item_id = OLD.id AND status IN ('queued', 'running', 'ready')
     ORDER BY user_id, id FOR UPDATE;
    FOR package_row IN
        SELECT id FROM offline_packages
         WHERE item_id = OLD.id AND status IN ('queued', 'running', 'ready')
         ORDER BY user_id, id
    LOOP
        UPDATE offline_packages
           SET status = 'cancelled', reservation_bytes = 0, actual_size = NULL,
               sha256 = NULL, relative_path = NULL, claimed_run_id = NULL,
               error_code = 'source-unavailable', finished_at = NOW(), updated_at = NOW()
         WHERE id = package_row.id;
    END LOOP;
    RETURN OLD;
END;
$$;
CREATE TRIGGER items_cancel_offline_packages_before_delete
    BEFORE DELETE ON items
    FOR EACH ROW EXECUTE FUNCTION cancel_offline_packages_for_item_delete();
