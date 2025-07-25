---
title: "REASSIGN OWNED"
description: "`REASSIGN OWNED` reassigns the owner of all the objects that are owned by one of the specified roles."
menu:
  main:
    parent: commands
---

`REASSIGN OWNED` reassigns the owner of all the objects that are owned by one of the specified roles.

{{< note >}}
Unlike [PostgreSQL](https://www.postgresql.org/docs/current/sql-drop-owned.html), Materialize reassigns
all objects across all databases, including the databases themselves.
{{< /note >}}

## Syntax

{{< diagram "reassign-owned.svg" >}}

Field | Use
------|-----
_old_role_ | The role name whose owned objects will be reassigned.
_new_role_ | The role name of the new owner of all the objects.

## Examples

```mzsql
REASSIGN OWNED BY joe TO mike;
```

```mzsql
REASSIGN OWNED BY joe, george TO mike;
```

## Privileges

The privileges required to execute this statement are:

{{< include-md file="shared-content/sql-command-privileges/reassign-owned.md"
>}}

## Related pages

- [`ALTER OWNER`](../alter-owner)
- [`DROP OWNED`](../drop-owned)
