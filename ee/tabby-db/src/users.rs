use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use sqlx::{query, query_scalar, FromRow};
use uuid::Uuid;

use super::DbConn;
use crate::SQLXResultExt;

#[allow(unused)]
#[derive(FromRow)]
pub struct UserDAO {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,

    pub id: i32,
    pub email: String,
    pub password_encrypted: String,
    pub is_admin: bool,

    /// To authenticate IDE extensions / plugins to access code completion / chat api endpoints.
    pub auth_token: String,
    pub active: bool,
}

static OWNER_USER_ID: i32 = 1;

impl UserDAO {
    fn select(clause: &str) -> String {
        r#"SELECT id, email, password_encrypted, is_admin, created_at, updated_at, auth_token, active FROM users WHERE "#
            .to_owned()
            + clause
    }

    pub fn is_owner(&self) -> bool {
        self.id == OWNER_USER_ID
    }
}

/// db read/write operations for `users` table
impl DbConn {
    pub async fn create_user(
        &self,
        email: String,
        password_encrypted: String,
        is_admin: bool,
    ) -> Result<i32> {
        self.create_user_impl(email, password_encrypted, is_admin, None)
            .await
    }

    pub async fn create_user_with_invitation(
        &self,
        email: String,
        password_encrypted: String,
        is_admin: bool,
        invitation_id: i32,
    ) -> Result<i32> {
        self.create_user_impl(email, password_encrypted, is_admin, Some(invitation_id))
            .await
    }

    async fn create_user_impl(
        &self,
        email: String,
        password_encrypted: String,
        is_admin: bool,
        invitation_id: Option<i32>,
    ) -> Result<i32> {
        let mut transaction = self.pool.begin().await?;
        if let Some(invitation_id) = invitation_id {
            query!("DELETE FROM invitations WHERE id = ?", invitation_id)
                .execute(&mut *transaction)
                .await?;
        }
        let token = generate_auth_token();
        let res = query!("INSERT INTO users (email, password_encrypted, is_admin, auth_token) VALUES (?, ?, ?, ?)",
            email, password_encrypted, is_admin, token)
            .execute(&mut *transaction).await;
        let res = res.unique_error("User already exists")?;
        transaction.commit().await?;

        self.cache.active_user_count.invalidate().await;
        if is_admin {
            self.cache.active_admin_count.invalidate().await;
        }

        Ok(res.last_insert_rowid() as i32)
    }

    pub async fn get_user(&self, id: i32) -> Result<Option<UserDAO>> {
        let user = sqlx::query_as(&UserDAO::select("id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(user)
    }

    pub async fn get_user_by_email(&self, email: &str) -> Result<Option<UserDAO>> {
        let user = sqlx::query_as(&UserDAO::select("email = ?"))
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;

        Ok(user)
    }

    pub async fn list_admin_users(&self) -> Result<Vec<UserDAO>> {
        let users = sqlx::query_as(&UserDAO::select("is_admin"))
            .fetch_all(&self.pool)
            .await?;

        Ok(users)
    }

    pub async fn list_users_with_filter(
        &self,
        limit: Option<usize>,
        skip_id: Option<i32>,
        backwards: bool,
    ) -> Result<Vec<UserDAO>> {
        let query = Self::make_pagination_query(
            "users",
            &[
                "id",
                "email",
                "password_encrypted",
                "is_admin",
                "created_at",
                "updated_at",
                "auth_token",
                "active",
            ],
            limit,
            skip_id,
            backwards,
        );

        let users = sqlx::query_as(&query).fetch_all(&self.pool).await?;
        Ok(users)
    }

    pub async fn verify_auth_token(&self, token: &str, requires_owner: bool) -> Result<i64> {
        let token = token.to_owned();
        let Some(id) = query_scalar!(
            "SELECT id FROM users WHERE auth_token = ? AND active AND (id == ? OR NOT ?)",
            token,
            OWNER_USER_ID,
            requires_owner
        )
        .fetch_one(&self.pool)
        .await?
        else {
            bail!("Invalid auth_token")
        };

        Ok(id)
    }

    pub async fn reset_user_auth_token_by_id(&self, id: i32) -> Result<()> {
        let updated_at = chrono::Utc::now();
        let token = generate_auth_token();
        query!(
            r#"UPDATE users SET auth_token = ?, updated_at = ? WHERE id = ?"#,
            token,
            updated_at,
            id
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn update_user_active(&self, id: i32, active: bool) -> Result<()> {
        let not_active = !active;
        let changed = query!(
            "UPDATE users SET active = ? WHERE id = ? AND active = ?",
            active,
            id,
            not_active
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(anyhow!("user active status was not changed"));
        }
        self.cache.active_admin_count.invalidate().await;
        self.cache.active_user_count.invalidate().await;
        Ok(())
    }

    pub async fn update_user_role(&self, id: i32, is_admin: bool) -> Result<()> {
        let not_admin = !is_admin;
        let changed = query!(
            "UPDATE users SET is_admin = ? WHERE id = ? AND is_admin = ?",
            is_admin,
            id,
            not_admin
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed != 1 {
            Err(anyhow!("user admin status was not changed"))
        } else {
            self.cache.active_admin_count.invalidate().await;
            Ok(())
        }
    }

    pub async fn update_user_password(&self, id: i32, password_encrypted: String) -> Result<()> {
        query!(
            "UPDATE users SET password_encrypted = ? WHERE id = ?",
            password_encrypted,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn count_active_users(&self) -> Result<usize> {
        self.cache
            .active_user_count
            .get_or_refresh(|| async {
                let users = query_scalar!("SELECT COUNT(1) FROM users WHERE active;")
                    .fetch_one(&self.pool)
                    .await?;
                Ok(users as usize)
            })
            .await
    }

    pub async fn count_active_admin_users(&self) -> Result<usize> {
        self.cache
            .active_admin_count
            .get_or_refresh(|| async {
                let users = query_scalar!("SELECT COUNT(1) FROM users WHERE active and is_admin;")
                    .fetch_one(&self.pool)
                    .await?;
                Ok(users as usize)
            })
            .await
    }
}

fn generate_auth_token() -> String {
    let uuid = Uuid::new_v4().to_string().replace('-', "");
    format!("auth_{}", uuid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::create_user;

    #[tokio::test]
    async fn test_create_user() {
        let conn = DbConn::new_in_memory().await.unwrap();

        let id = create_user(&conn).await;
        let user = conn.get_user(id).await.unwrap().unwrap();
        assert_eq!(user.id, 1);
    }

    #[tokio::test]
    async fn test_set_active() {
        let conn = DbConn::new_in_memory().await.unwrap();
        let id = create_user(&conn).await;

        assert!(conn.get_user(id).await.unwrap().unwrap().active);

        conn.update_user_active(id, false).await.unwrap();

        assert!(!conn.get_user(id).await.unwrap().unwrap().active);

        // Setting an already inactive user to inactive should error
        assert!(conn.update_user_active(id, false).await.is_err());
    }

    #[tokio::test]
    async fn test_get_user_by_email() {
        let conn = DbConn::new_in_memory().await.unwrap();

        let email = "hello@example.com";
        let user = conn.get_user_by_email(email).await.unwrap();

        assert!(user.is_none());
    }

    #[tokio::test]
    async fn test_auth_token() {
        let conn = DbConn::new_in_memory().await.unwrap();
        let id = create_user(&conn).await;

        let user = conn.get_user(id).await.unwrap().unwrap();

        assert!(conn.verify_auth_token("abcd", false).await.is_err());

        assert!(conn
            .verify_auth_token(&user.auth_token, false)
            .await
            .is_ok());

        conn.reset_user_auth_token_by_id(user.id).await.unwrap();
        let new_user = conn.get_user(id).await.unwrap().unwrap();
        assert_eq!(user.email, new_user.email);
        assert_ne!(user.auth_token, new_user.auth_token);

        // Inactive user's auth token will be rejected.
        conn.update_user_active(new_user.id, false).await.unwrap();
        assert!(conn
            .verify_auth_token(&new_user.auth_token, false)
            .await
            .is_err());

        // Owner user should pass verification.
        assert!(conn
            .verify_auth_token(&new_user.auth_token, true)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_list_users_with_filter() {
        let conn = DbConn::new_in_memory().await.unwrap();

        let empty: Vec<i32> = vec![];
        let to_ids = |users: Vec<UserDAO>| users.into_iter().map(|u| u.id).collect::<Vec<_>>();

        // empty
        // forwards
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(None, None, false)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(Some(2), None, false)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(None, Some(1), false)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(Some(2), Some(1), false)
                    .await
                    .unwrap()
            )
        );
        // backwards
        assert_eq!(
            empty,
            to_ids(conn.list_users_with_filter(None, None, true).await.unwrap())
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(Some(2), None, true)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(None, Some(1), true)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(Some(1), Some(1), true)
                    .await
                    .unwrap()
            )
        );

        let id1 = conn
            .create_user("use1@example.com".into(), "123456".into(), false)
            .await
            .unwrap();

        // one user
        // forwards
        assert_eq!(
            vec![id1],
            to_ids(
                conn.list_users_with_filter(None, None, false)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            vec![id1],
            to_ids(
                conn.list_users_with_filter(Some(2), None, false)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(None, Some(1), false)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(Some(2), Some(1), false)
                    .await
                    .unwrap()
            )
        );
        // backwards
        assert_eq!(
            vec![id1],
            to_ids(conn.list_users_with_filter(None, None, true).await.unwrap())
        );
        assert_eq!(
            vec![id1],
            to_ids(
                conn.list_users_with_filter(Some(2), None, true)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(None, Some(1), true)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            empty,
            to_ids(
                conn.list_users_with_filter(Some(1), Some(1), true)
                    .await
                    .unwrap()
            )
        );

        let id2 = conn
            .create_user("use2@example.com".into(), "123456".into(), false)
            .await
            .unwrap();
        let id3 = conn
            .create_user("use3@example.com".into(), "123456".into(), false)
            .await
            .unwrap();
        let id4 = conn
            .create_user("use4@example.com".into(), "123456".into(), false)
            .await
            .unwrap();
        let id5 = conn
            .create_user("use5@example.com".into(), "123456".into(), false)
            .await
            .unwrap();

        // multiple users
        // forwards
        assert_eq!(
            vec![id1, id2, id3, id4, id5],
            to_ids(
                conn.list_users_with_filter(None, None, false)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            vec![id1, id2],
            to_ids(
                conn.list_users_with_filter(Some(2), None, false)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            vec![id3, id4, id5],
            to_ids(
                conn.list_users_with_filter(None, Some(2), false)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            vec![id3, id4],
            to_ids(
                conn.list_users_with_filter(Some(2), Some(2), false)
                    .await
                    .unwrap()
            )
        );
        // backwards
        assert_eq!(
            vec![id1, id2, id3, id4, id5],
            to_ids(conn.list_users_with_filter(None, None, true).await.unwrap())
        );
        assert_eq!(
            vec![id4, id5],
            to_ids(
                conn.list_users_with_filter(Some(2), None, true)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            vec![id1, id2, id3],
            to_ids(
                conn.list_users_with_filter(None, Some(4), true)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            vec![id2, id3],
            to_ids(
                conn.list_users_with_filter(Some(2), Some(4), true)
                    .await
                    .unwrap()
            )
        );
    }

    #[tokio::test]
    async fn test_caching() {
        let db = DbConn::new_in_memory().await.unwrap();

        db.create_user("example@example.com".into(), "".into(), true)
            .await
            .unwrap();

        assert_eq!(db.count_active_users().await.unwrap(), 1);
        assert_eq!(db.count_active_admin_users().await.unwrap(), 1);

        let user2_id = db
            .create_user("example2@example.com".into(), "".into(), false)
            .await
            .unwrap();
        assert_eq!(db.count_active_users().await.unwrap(), 2);
        assert_eq!(db.count_active_admin_users().await.unwrap(), 1);

        db.update_user_active(user2_id, false).await.unwrap();
        assert_eq!(db.count_active_users().await.unwrap(), 1);
        assert_eq!(db.count_active_admin_users().await.unwrap(), 1);

        let user3_id = db
            .create_user("example3@example.com".into(), "".into(), true)
            .await
            .unwrap();
        assert_eq!(db.count_active_users().await.unwrap(), 2);
        assert_eq!(db.count_active_admin_users().await.unwrap(), 2);

        db.update_user_active(user3_id, false).await.unwrap();
        assert_eq!(db.count_active_users().await.unwrap(), 1);
        assert_eq!(db.count_active_admin_users().await.unwrap(), 1);
    }
}
