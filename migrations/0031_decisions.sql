create table decision_statuses (
    relation_id bigint primary key references relations(id) on delete cascade,
    before text not null
);

create table decision_overrides (
    entity_id bigint primary key references entities(id) on delete cascade,
    class_iri text,
    name text,
    method text not null,
    status text not null,
    confidence double precision not null,
    ontology_version_id bigint not null references ontology_versions(id)
);

create table hidden_attributes (
    attribute_id bigint primary key references attributes(id) on delete cascade
);
