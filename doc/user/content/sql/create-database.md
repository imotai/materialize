---
title: "CREATE DATABASE"
description: "`CREATE DATABASE` creates a new database."
menu:
  main:
    parent: 'commands'
---

`CREATE DATABASE` creates a new database.

## Conceptual framework

Materialize mimics SQL standard's namespace hierarchy, which is:

- Databases (highest level)
- Schemas
- Tables, views, sources
- Columns (lowest level)

Each layer in the hierarchy can contain elements directly beneath it. In this
instance, databases can contain schemas.

For more information, see [Namespaces](../namespaces).

## Syntax

{{< diagram "create-database.svg" >}}

Field | Use
------|-----
**IF NOT EXISTS** | If specified, _do not_ generate an error if a database of the same name already exists. <br/><br/>If _not_ specified, throw an error if a database of the same name already exists. _(Default)_
_database&lowbar;name_ | A name for the database.

## Details

For details about databases, see [Namespaces: Database
details](../namespaces/#database-details).

## Examples

```mzsql
CREATE DATABASE IF NOT EXISTS my_db;
```
```mzsql
SHOW DATABASES;
```
```nofmt
materialize
my_db
```

## Privileges

The privileges required to execute this statement are:

{{< include-md file="shared-content/sql-command-privileges/create-database.md"
>}}

## Related pages

- [`DROP DATABASE`](../drop-database)
- [`SHOW DATABASES`](../show-databases)
