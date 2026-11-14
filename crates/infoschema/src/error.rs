//! go-file: pkg/infoschema/error.go
//!
//! The error variables of package infoschema (no functions).

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::{ClassExecutor, ClassSchema};
use tidb_base::errno;

/// ErrDatabaseExists returns for database already exists.
pub static ErrDatabaseExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrDBCreateExists));
/// ErrDatabaseDropExists returns for dropping a non-existent database.
pub static ErrDatabaseDropExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrDBDropExists));
/// ErrAccessDenied return when the user doesn't have the permission to access the table.
pub static ErrAccessDenied: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrAccessDenied));
/// ErrDatabaseNotExists returns for database not exists.
pub static ErrDatabaseNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrBadDB));
/// ErrPlacementPolicyExists returns for placement_policy policy already exists.
pub static ErrPlacementPolicyExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrPlacementPolicyExists));
/// ErrPlacementPolicyNotExists return for placement_policy policy not exists.
pub static ErrPlacementPolicyNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrPlacementPolicyNotExists));
/// ErrResourceGroupExists return for resource group already exists.
pub static ErrResourceGroupExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrResourceGroupExists));
/// ErrResourceGroupNotExists return for resource group not exists.
pub static ErrResourceGroupNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrResourceGroupNotExists));
/// ErrResourceGroupInvalidBackgroundTaskName return for unknown resource group background task name.
pub static ErrResourceGroupInvalidBackgroundTaskName: LazyLock<Error> =
    LazyLock::new(|| ClassExecutor.new_std(errno::ErrResourceGroupInvalidBackgroundTaskName));
/// ErrResourceGroupInvalidForRole return for invalid resource group for role.
pub static ErrResourceGroupInvalidForRole: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrResourceGroupInvalidForRole));
/// ErrReservedSyntax for internal syntax.
pub static ErrReservedSyntax: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrReservedSyntax));
/// ErrTableExists returns for table already exists.
pub static ErrTableExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrTableExists));
/// ErrTableDropExists returns for dropping a non-existent table.
pub static ErrTableDropExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrBadTable));
/// ErrSequenceDropExists returns for dropping a non-exist sequence.
pub static ErrSequenceDropExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrUnknownSequence));
/// ErrColumnNotExists returns for column not exists.
pub static ErrColumnNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrBadField));
/// ErrColumnExists returns for column already exists.
pub static ErrColumnExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrDupFieldName));
/// ErrKeyNameDuplicate returns for index duplicate when rename index.
pub static ErrKeyNameDuplicate: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrDupKeyName));
/// ErrNonuniqTable returns when none unique tables errors.
pub static ErrNonuniqTable: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrNonuniqTable));
/// ErrMultiplePriKey returns for multiple primary keys.
pub static ErrMultiplePriKey: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrMultiplePriKey));
/// ErrTooManyKeyParts returns for too many key parts.
pub static ErrTooManyKeyParts: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrTooManyKeyParts));
/// ErrForeignKeyNotExists returns for foreign key not exists.
pub static ErrForeignKeyNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrCantDropFieldOrKey));
/// ErrTableNotLockedForWrite returns for write tables when only hold the table read lock.
pub static ErrTableNotLockedForWrite: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrTableNotLockedForWrite));
/// ErrTableNotLocked returns when session has explicitly lock tables, then visit unlocked table will return this error.
pub static ErrTableNotLocked: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrTableNotLocked));
/// ErrTableNotExists returns for table not exists.
pub static ErrTableNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrNoSuchTable));
/// ErrKeyNotExists returns for index not exists.
pub static ErrKeyNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrKeyDoesNotExist));
/// ErrCannotAddForeign returns for foreign key exists.
pub static ErrCannotAddForeign: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrCannotAddForeign));
/// ErrForeignKeyOnPartitioned returns for foreign key on partition table.
pub static ErrForeignKeyOnPartitioned: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrForeignKeyOnPartitioned));
/// ErrForeignKeyNotMatch returns for foreign key not match.
pub static ErrForeignKeyNotMatch: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrWrongFkDef));
/// ErrIndexExists returns for index already exists.
pub static ErrIndexExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrDupIndex));
/// ErrUserDropExists returns for dropping a non-existent user.
pub static ErrUserDropExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrBadUser));
/// ErrUserAlreadyExists return for creating a existent user.
pub static ErrUserAlreadyExists: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrUserAlreadyExists));
/// ErrTableLocked returns when the table was locked by other session.
pub static ErrTableLocked: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrTableLocked));
/// ErrWrongObject returns when the table/view/sequence is not the expected object.
pub static ErrWrongObject: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrWrongObject));
/// ErrAdminCheckTable returns when the check table in temporary mode.
pub static ErrAdminCheckTable: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrAdminCheckTable));
/// ErrEmptyDatabase returns when the database is unexpectedly empty.
pub static ErrEmptyDatabase: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrBadDB));
/// ErrForbidSchemaChange returns when the schema change is illegal
pub static ErrForbidSchemaChange: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrForbidSchemaChange));
/// ErrTableWithoutPrimaryKey returns when there is no primary key on a table and sql_require_primary_key is set
pub static ErrTableWithoutPrimaryKey: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrTableWithoutPrimaryKey));
/// ErrForeignKeyCannotUseVirtualColumn returns when foreign key refer virtual generated column.
pub static ErrForeignKeyCannotUseVirtualColumn: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrForeignKeyCannotUseVirtualColumn));
/// ErrForeignKeyCannotOpenParent returns when foreign key refer table not exists.
pub static ErrForeignKeyCannotOpenParent: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrForeignKeyCannotOpenParent));
/// ErrForeignKeyNoColumnInParent returns when foreign key refer columns don't exist in parent table.
pub static ErrForeignKeyNoColumnInParent: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrForeignKeyNoColumnInParent));
/// ErrForeignKeyNoIndexInParent returns when foreign key refer columns don't have related index in parent table.
pub static ErrForeignKeyNoIndexInParent: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrForeignKeyNoIndexInParent));
/// ErrForeignKeyColumnNotNull returns when foreign key with SET NULL constrain and the related column has not null.
pub static ErrForeignKeyColumnNotNull: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrForeignKeyColumnNotNull));
/// ErrResourceGroupSupportDisabled returns for resource group feature is disabled
pub static ErrResourceGroupSupportDisabled: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrResourceGroupSupportDisabled));
/// ErrCheckConstraintDupName returns for duplicate constraint names.
pub static ErrCheckConstraintDupName: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrCheckConstraintDupName));
/// ErrProtectedTableMode returns for accessing table in import/restore mode.
pub static ErrProtectedTableMode: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrProtectedTableMode));
/// ErrInvalidTableModeSet returns for invalid TableMode conversion.
pub static ErrInvalidTableModeSet: LazyLock<Error> =
    LazyLock::new(|| ClassSchema.new_std(errno::ErrInvalidTableModeSet));
