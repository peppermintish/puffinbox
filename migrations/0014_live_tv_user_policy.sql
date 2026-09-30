ALTER TABLE users
    ADD COLUMN enable_live_tv_access BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN enable_live_tv_management BOOLEAN NOT NULL DEFAULT FALSE;
