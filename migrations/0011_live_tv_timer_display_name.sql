ALTER TABLE live_tv_timers
    ADD COLUMN display_name TEXT;

ALTER TABLE live_tv_timers
    ADD CONSTRAINT live_tv_timers_display_name_valid
    CHECK (
        display_name IS NULL
        OR (
            length(display_name) BETWEEN 1 AND 512
            AND display_name !~ '[[:cntrl:]]'
        )
    );
