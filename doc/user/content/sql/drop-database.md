---
title: "DROP DATABASE"
description: "`DROP DATABASE` removes a database from Materialize."
menu:
  main:
    parent: 'commands'
---

`DROP DATABASE` removes a database from Materialize.

{{< warning >}} `DROP DATABASE` immediately removes all objects within the
database without confirmation. Use with care! {{< /warning >}}

## Syntax

{{< diagram "drop-database.svg" >}}

Field | Use
------|-----
**IF EXISTS** | Do not return an error if the specified database does not exist.
_database&lowbar;name_ | The database you want to drop. For available databases, see [`SHOW DATABASES`](../show-databases).
**CASCADE** | Remove the database and its dependent objects. _(Default)_
**RESTRICT** | Do not remove this database if it contains any schemas.

## Example

### Remove a database containing schemas
You can use either of the following commands:

- ```mzsql
  DROP DATABASE my_db;
  ```
- ```mzsql
  DROP DATABASE my_db CASCADE;
  ```

### Remove a database only if it contains no schemas
```mzsql
DROP DATABASE my_db RESTRICT;
```

### Do not issue an error if attempting to remove a nonexistent database
```mzsql
DROP DATABASE IF EXISTS my_db;
```

## Privileges

The privileges required to execute this statement are:

{{< include-md file="shared-content/sql-command-privileges/drop-database.md" >}}

## Related pages

- [`DROP OWNED`](../drop-owned)
