// SPDX-License-Identifier: Apache-2.0

//! Schema setup. The DDL produced here matches `docs/indexer-schema.md` v0.1.
//!
//! Tables are created without indexes — indexes are created after data load
//! for faster bulk insert (see `create_indexes`).

use duckdb::Connection;

use crate::error::{IndexError, Result};

/// Drop any existing indexer tables. Used to make `index()` idempotent —
/// the user-facing contract is "running indexer twice gives the same DB."
const DROP_ALL: &str = r#"
    DROP TABLE IF EXISTS event_locals;
    DROP TABLE IF EXISTS event_args;
    DROP TABLE IF EXISTS note_kwargs;
    DROP TABLE IF EXISTS branches;
    DROP TABLE IF EXISTS exceptions;
    DROP TABLE IF EXISTS notes;
    DROP TABLE IF EXISTS scope_boundaries;
    DROP TABLE IF EXISTS value_elements;
    DROP TABLE IF EXISTS values;
    DROP TABLE IF EXISTS frames;
    DROP TABLE IF EXISTS events;
    DROP TABLE IF EXISTS source_files;
    DROP TABLE IF EXISTS recorded_functions;
    DROP TABLE IF EXISTS excluded_functions;
    DROP TABLE IF EXISTS trace_metadata;
"#;

const CREATE_TABLES: &str = r#"
    CREATE TABLE events (
        event_id              BIGINT PRIMARY KEY,
        type                  VARCHAR NOT NULL,
        frame_id              BIGINT NOT NULL,
        timestamp_ns          BIGINT NOT NULL,
        source_file           VARCHAR,
        line                  INTEGER,
        function_name         VARCHAR,
        return_value_id       BIGINT,
        branch_taken          BOOLEAN,
        exception_type        VARCHAR,
        exception_value_id    BIGINT,
        note_message          VARCHAR,
        boundary_type         VARCHAR,
        boundary_reason       VARCHAR
    );

    CREATE TABLE frames (
        frame_id              BIGINT PRIMARY KEY,
        function_name         VARCHAR NOT NULL,
        qualified_name        VARCHAR NOT NULL,
        source_file           VARCHAR NOT NULL,
        parent_frame_id       BIGINT,
        entry_event_id        BIGINT NOT NULL,
        exit_event_id         BIGINT,
        exit_kind             VARCHAR,
        depth                 INTEGER NOT NULL,
        call_index            INTEGER NOT NULL,
        duration_ns           BIGINT,
        argument_summary      VARCHAR
    );

    CREATE TABLE event_locals (
        event_id              BIGINT NOT NULL,
        frame_id              BIGINT NOT NULL,
        name                  VARCHAR NOT NULL,
        value_id              BIGINT NOT NULL,
        PRIMARY KEY (event_id, name)
    );

    CREATE TABLE event_args (
        event_id              BIGINT NOT NULL,
        position              INTEGER NOT NULL,
        name                  VARCHAR NOT NULL,
        value_id              BIGINT NOT NULL,
        PRIMARY KEY (event_id, position)
    );

    CREATE TABLE note_kwargs (
        event_id              BIGINT NOT NULL,
        name                  VARCHAR NOT NULL,
        value_id              BIGINT NOT NULL,
        PRIMARY KEY (event_id, name)
    );

    CREATE TABLE values (
        value_id              BIGINT PRIMARY KEY,
        type_tag              VARCHAR NOT NULL,
        hash_kind             VARCHAR NOT NULL,
        hash_hex              VARCHAR NOT NULL,
        bool_value            BOOLEAN,
        int_value             BIGINT,
        big_int_hex           VARCHAR,
        float_value           DOUBLE,
        string_value          VARCHAR,
        bytes_value           BLOB,
        container_length      BIGINT,
        cycle_ref_depth       INTEGER,
        type_name             VARCHAR,
        repr_text             VARCHAR,
        summary_length        BIGINT,
        type_ref_name         VARCHAR
    );

    CREATE TABLE value_elements (
        container_value_id    BIGINT NOT NULL,
        position              INTEGER NOT NULL,
        key_value_id          BIGINT,
        element_value_id      BIGINT NOT NULL,
        PRIMARY KEY (container_value_id, position)
    );

    CREATE TABLE source_files (
        path                  VARCHAR PRIMARY KEY,
        content_hash          VARCHAR NOT NULL,
        content               TEXT NOT NULL,
        line_count            INTEGER NOT NULL
    );

    CREATE TABLE branches (
        event_id              BIGINT PRIMARY KEY,
        frame_id              BIGINT NOT NULL,
        function_name         VARCHAR NOT NULL,
        source_file           VARCHAR NOT NULL,
        line                  INTEGER NOT NULL,
        taken                 BOOLEAN NOT NULL,
        timestamp_ns          BIGINT NOT NULL
    );

    CREATE TABLE exceptions (
        event_id              BIGINT PRIMARY KEY,
        frame_id              BIGINT NOT NULL,
        function_name         VARCHAR NOT NULL,
        source_file           VARCHAR NOT NULL,
        line                  INTEGER NOT NULL,
        exception_type        VARCHAR NOT NULL,
        exception_value_id    BIGINT NOT NULL,
        timestamp_ns          BIGINT NOT NULL
    );

    CREATE TABLE notes (
        event_id              BIGINT PRIMARY KEY,
        frame_id              BIGINT NOT NULL,
        function_name         VARCHAR NOT NULL,
        source_file           VARCHAR NOT NULL,
        line                  INTEGER NOT NULL,
        message               VARCHAR NOT NULL,
        timestamp_ns          BIGINT NOT NULL
    );

    CREATE TABLE scope_boundaries (
        event_id              BIGINT PRIMARY KEY,
        frame_id              BIGINT NOT NULL,
        boundary_type         VARCHAR NOT NULL,
        reason                VARCHAR,
        timestamp_ns          BIGINT NOT NULL
    );

    CREATE TABLE trace_metadata (
        recorder_language     VARCHAR NOT NULL,
        recorder_version      VARCHAR NOT NULL,
        language_version      VARCHAR NOT NULL,
        platform              VARCHAR NOT NULL,
        program               VARCHAR NOT NULL,
        working_directory     VARCHAR,
        trace_uuid            VARCHAR NOT NULL,
        recording_start_ns    BIGINT NOT NULL,
        recording_end_ns      BIGINT,
        include_patterns      VARCHAR,
        exclude_patterns      VARCHAR,
        depth_limit           INTEGER,
        skip_blocks_observed  INTEGER,
        depth_clips_observed  INTEGER,
        total_events          BIGINT,
        total_blocks          INTEGER,
        trace_duration_ns     BIGINT,
        function_entry_count  BIGINT,
        line_event_count      BIGINT,
        branch_event_count    BIGINT,
        exception_event_count BIGINT,
        note_event_count      BIGINT
    );

    CREATE TABLE recorded_functions (
        qualified_name VARCHAR PRIMARY KEY
    );

    CREATE TABLE excluded_functions (
        qualified_name VARCHAR PRIMARY KEY,
        matched_pattern VARCHAR NOT NULL
    );
"#;

const CREATE_INDEXES: &str = r#"
    CREATE INDEX events_frame_id ON events(frame_id);
    CREATE INDEX events_type ON events(type);
    CREATE INDEX events_function_name ON events(function_name);
    CREATE INDEX events_source_line ON events(source_file, line);
    CREATE INDEX events_timestamp ON events(timestamp_ns);

    CREATE INDEX frames_qualified_name ON frames(qualified_name);
    CREATE INDEX frames_function_name ON frames(function_name);
    CREATE INDEX frames_parent_frame_id ON frames(parent_frame_id);
    CREATE INDEX frames_source_file ON frames(source_file);

    CREATE INDEX event_locals_frame_name ON event_locals(frame_id, name);
    CREATE INDEX event_locals_value_id ON event_locals(value_id);
    CREATE INDEX event_locals_name ON event_locals(name);

    CREATE INDEX event_args_event_id ON event_args(event_id);

    CREATE INDEX note_kwargs_event_id ON note_kwargs(event_id);

    CREATE INDEX values_type_tag ON values(type_tag);
    CREATE INDEX values_hash ON values(hash_kind, hash_hex);
    CREATE INDEX values_type_name ON values(type_name);
    CREATE INDEX values_int_value ON values(int_value);
    CREATE INDEX values_string_value ON values(string_value);

    CREATE INDEX value_elements_container ON value_elements(container_value_id);
    CREATE INDEX value_elements_element ON value_elements(element_value_id);
    CREATE INDEX value_elements_key ON value_elements(key_value_id);

    CREATE INDEX branches_frame_line ON branches(frame_id, line);
    CREATE INDEX branches_source_line ON branches(source_file, line);
    CREATE INDEX branches_function ON branches(function_name);

    CREATE INDEX exceptions_type ON exceptions(exception_type);
    CREATE INDEX exceptions_frame ON exceptions(frame_id);
    CREATE INDEX exceptions_function ON exceptions(function_name);

    CREATE INDEX notes_frame ON notes(frame_id);
    CREATE INDEX notes_function ON notes(function_name);

    CREATE INDEX scope_boundaries_frame ON scope_boundaries(frame_id);
    CREATE INDEX scope_boundaries_type ON scope_boundaries(boundary_type);
"#;

pub fn drop_all(conn: &Connection) -> Result<()> {
    conn.execute_batch(DROP_ALL).map_err(IndexError::from)
}

pub fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(CREATE_TABLES).map_err(IndexError::from)
}

pub fn create_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(CREATE_INDEXES).map_err(IndexError::from)
}
