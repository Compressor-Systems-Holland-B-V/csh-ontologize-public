alter table fact_reviews
    add column reviewer text not null default 'human' check (reviewer in ('human', 'judge')),
    add column planted boolean not null default false;

create index fact_reviews_reviewer on fact_reviews (reviewer, kind);
