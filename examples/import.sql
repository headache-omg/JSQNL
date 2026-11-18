CREATE TABLE users (
    id BIGINT PRIMARY KEY,
    name VARCHAR(80) NOT NULL,
    active BOOLEAN,
    profile JSON
);
