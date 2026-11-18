create table document_names (
    document_id bigint primary key references documents (id) on delete cascade,
    tsv tsvector not null
);

create index document_names_tsv on document_names using gin (tsv);
