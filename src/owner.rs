use diesel::prelude::*;

use app::App;
use github;
use schema::*;
use util::{human, CargoResult};
use {Crate, User};

#[derive(Insertable, Associations, Identifiable, Debug, Clone, Copy)]
#[belongs_to(Crate)]
#[belongs_to(User, foreign_key = "owner_id")]
#[belongs_to(Team, foreign_key = "owner_id")]
#[table_name = "crate_owners"]
#[primary_key(crate_id, owner_id, owner_kind)]
pub struct CrateOwner {
    pub crate_id: i32,
    pub owner_id: i32,
    pub created_by: i32,
    pub owner_kind: i32,
}

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
pub enum OwnerKind {
    User = 0,
    Team = 1,
}

/// Unifies the notion of a User or a Team.
#[derive(Debug)]
pub enum Owner {
    User(User),
    Team(Team),
}

/// For now, just a Github Team. Can be upgraded to other teams
/// later if desirable.
#[derive(Queryable, Identifiable, Serialize, Deserialize, Debug)]
pub struct Team {
    /// Unique table id
    pub id: i32,
    /// "github:org:team"
    /// An opaque unique ID, that was at one point parsed out to query Github.
    /// We only query membership with github using the github_id, though.
    /// This is the only name we should ever talk to Cargo about.
    pub login: String,
    /// The GitHub API works on team ID numbers. This can change, if a team
    /// is deleted and then recreated with the same name!!!
    pub github_id: i32,
    /// Sugary goodness
    pub name: Option<String>,
    pub avatar: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct EncodableTeam {
    pub id: i32,
    pub login: String,
    pub name: Option<String>,
    pub avatar: Option<String>,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EncodableOwner {
    pub id: i32,
    pub login: String,
    pub kind: String,
    pub url: Option<String>,
    pub name: Option<String>,
    pub avatar: Option<String>,
}

/// Access rights to the crate (publishing and ownership management)
/// NOTE: The order of these variants matters!
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone, Copy)]
pub enum Rights {
    None,
    Publish,
    Full,
}

#[derive(Insertable, AsChangeset, Debug)]
#[table_name = "teams"]
pub struct NewTeam<'a> {
    pub login: &'a str,
    pub github_id: i32,
    pub name: Option<String>,
    pub avatar: Option<String>,
}

impl<'a> NewTeam<'a> {
    pub fn new(
        login: &'a str,
        github_id: i32,
        name: Option<String>,
        avatar: Option<String>,
    ) -> Self {
        NewTeam {
            login: login,
            github_id: github_id,
            name: name,
            avatar: avatar,
        }
    }

    pub fn create_or_update(&self, conn: &PgConnection) -> QueryResult<Team> {
        use diesel::insert_into;
        use schema::teams::dsl::*;

        insert_into(teams)
            .values(self)
            .on_conflict(login)
            .do_update()
            .set(self)
            .get_result(conn)
    }
}

impl Team {
    /// Tries to create the Team in the DB (assumes a `:` has already been found).
    pub fn create_or_update(
        app: &App,
        conn: &PgConnection,
        login: &str,
        req_user: &User,
    ) -> CargoResult<Self> {
        // must look like system:xxxxxxx
        let mut chunks = login.split(':');
        match chunks.next().unwrap() {
            // github:rust-lang:owners
            "github" => {
                // Ok to unwrap since we know one ":" is contained
                let org = chunks.next().unwrap();
                let team = chunks.next().ok_or_else(|| {
                    human(
                        "missing github team argument; \
                         format is github:org:team",
                    )
                })?;
                Team::create_or_update_github_team(app, conn, login, org, team, req_user)
            }
            _ => Err(human(
                "unknown organization handler, \
                 only 'github:org:team' is supported",
            )),
        }
    }

    /// Tries to create or update a Github Team. Assumes `org` and `team` are
    /// correctly parsed out of the full `name`. `name` is passed as a
    /// convenience to avoid rebuilding it.
    fn create_or_update_github_team(
        app: &App,
        conn: &PgConnection,
        login: &str,
        org_name: &str,
        team_name: &str,
        req_user: &User,
    ) -> CargoResult<Self> {
        // GET orgs/:org/teams
        // check that `team` is the `slug` in results, and grab its data

        // "sanitization"
        fn whitelist(c: &char) -> bool {
            match *c {
                'a'...'z' | 'A'...'Z' | '0'...'9' | '-' | '_' => false,
                _ => true,
            }
        }

        if let Some(c) = org_name.chars().find(whitelist) {
            return Err(human(&format_args!(
                "organization cannot contain special \
                 characters like {}",
                c
            )));
        }

        #[derive(Deserialize)]
        struct GithubTeam {
            slug: String,         // the name we want to find
            id: i32,              // unique GH id (needed for membership queries)
            name: Option<String>, // Pretty name
        }

        // FIXME: we just set per_page=100 and don't bother chasing pagination
        // links. A hundred teams should be enough for any org, right?
        let url = format!("/orgs/{}/teams?per_page=100", org_name);
        let token = github::token(req_user.gh_access_token.clone());
        let (handle, data) = github::github(app, &url, &token)?;
        let teams: Vec<GithubTeam> = github::parse_github_response(handle, &data)?;

        let team = teams
            .into_iter()
            .find(|team| team.slug == team_name)
            .ok_or_else(|| {
                human(&format_args!(
                    "could not find the github team {}/{}",
                    org_name, team_name
                ))
            })?;

        if !team_with_gh_id_contains_user(app, team.id, req_user)? {
            return Err(human("only members of a team can add it as an owner"));
        }

        #[derive(Deserialize)]
        struct Org {
            avatar_url: Option<String>,
        }

        let url = format!("/orgs/{}", org_name);
        let (handle, resp) = github::github(app, &url, &token)?;
        let org: Org = github::parse_github_response(handle, &resp)?;

        NewTeam::new(login, team.id, team.name, org.avatar_url)
            .create_or_update(conn)
            .map_err(Into::into)
    }

    /// Phones home to Github to ask if this User is a member of the given team.
    /// Note that we're assuming that the given user is the one interested in
    /// the answer. If this is not the case, then we could accidentally leak
    /// private membership information here.
    pub fn contains_user(&self, app: &App, user: &User) -> CargoResult<bool> {
        team_with_gh_id_contains_user(app, self.github_id, user)
    }

    pub fn owning(krate: &Crate, conn: &PgConnection) -> CargoResult<Vec<Owner>> {
        let base_query = CrateOwner::belonging_to(krate).filter(crate_owners::deleted.eq(false));
        let teams = base_query
            .inner_join(teams::table)
            .select(teams::all_columns)
            .filter(crate_owners::owner_kind.eq(OwnerKind::Team as i32))
            .load(conn)?
            .into_iter()
            .map(Owner::Team);

        Ok(teams.collect())
    }

    pub fn encodable(self) -> EncodableTeam {
        let Team {
            id,
            name,
            login,
            avatar,
            ..
        } = self;
        let url = Team::github_url(&login);

        EncodableTeam {
            id: id,
            login: login,
            name: name,
            avatar: avatar,
            url: Some(url),
        }
    }

    fn github_url(login: &str) -> String {
        let mut login_pieces = login.split(':');
        login_pieces.next();

        format!(
            "https://github.com/{}",
            login_pieces.next().expect("org failed"),
        )
    }
}

fn team_with_gh_id_contains_user(app: &App, github_id: i32, user: &User) -> CargoResult<bool> {
    // GET teams/:team_id/memberships/:user_name
    // check that "state": "active"

    #[derive(Deserialize)]
    struct Membership {
        state: String,
    }

    let url = format!("/teams/{}/memberships/{}", &github_id, &user.gh_login);
    let token = github::token(user.gh_access_token.clone());
    let (mut handle, resp) = github::github(app, &url, &token)?;

    // Officially how `false` is returned
    if handle.response_code().unwrap() == 404 {
        return Ok(false);
    }

    let membership: Membership = github::parse_github_response(handle, &resp)?;

    // There is also `state: pending` for which we could possibly give
    // some feedback, but it's not obvious how that should work.
    Ok(membership.state == "active")
}

impl Owner {
    /// Finds the owner by name. Always recreates teams to get the most
    /// up-to-date GitHub ID. Fails out if the user isn't found in the
    /// database, the team isn't found on GitHub, or if the user isn't a member
    /// of the team on GitHub.
    /// May be a user's GH login or a full team name. This is case
    /// sensitive.
    pub fn find_or_create_by_login(
        app: &App,
        conn: &PgConnection,
        req_user: &User,
        name: &str,
    ) -> CargoResult<Owner> {
        if name.contains(':') {
            Ok(Owner::Team(Team::create_or_update(
                app,
                conn,
                name,
                req_user,
            )?))
        } else {
            users::table
                .filter(users::gh_login.eq(name))
                .first(conn)
                .map(Owner::User)
                .map_err(|_| human(&format_args!("could not find user with login `{}`", name)))
        }
    }

    pub fn kind(&self) -> i32 {
        match *self {
            Owner::User(_) => OwnerKind::User as i32,
            Owner::Team(_) => OwnerKind::Team as i32,
        }
    }

    pub fn login(&self) -> &str {
        match *self {
            Owner::User(ref user) => &user.gh_login,
            Owner::Team(ref team) => &team.login,
        }
    }

    pub fn id(&self) -> i32 {
        match *self {
            Owner::User(ref user) => user.id,
            Owner::Team(ref team) => team.id,
        }
    }

    pub fn encodable(self) -> EncodableOwner {
        match self {
            Owner::User(User {
                id,
                name,
                gh_login,
                gh_avatar,
                ..
            }) => {
                let url = format!("https://github.com/{}", gh_login);
                EncodableOwner {
                    id: id,
                    login: gh_login,
                    avatar: gh_avatar,
                    url: Some(url),
                    name: name,
                    kind: String::from("user"),
                }
            }
            Owner::Team(Team {
                id,
                name,
                login,
                avatar,
                ..
            }) => {
                let url = Team::github_url(&login);
                EncodableOwner {
                    id: id,
                    login: login,
                    url: Some(url),
                    avatar: avatar,
                    name: name,
                    kind: String::from("team"),
                }
            }
        }
    }
}

/// Given this set of owners, determines the strongest rights the
/// given user has.
///
/// Shortcircuits on `Full` because you can't beat it. In practice we'll always
/// see `[user, user, user, ..., team, team, team]`, so we could shortcircuit on
/// `Publish` as well, but this is a non-obvious invariant so we don't bother.
/// Sweet free optimization if teams are proving burdensome to check.
/// More than one team isn't really expected, though.
pub fn rights(app: &App, owners: &[Owner], user: &User) -> CargoResult<Rights> {
    let mut best = Rights::None;
    for owner in owners {
        match *owner {
            Owner::User(ref other_user) => if other_user.id == user.id {
                return Ok(Rights::Full);
            },
            Owner::Team(ref team) => if team.contains_user(app, user)? {
                best = Rights::Publish;
            },
        }
    }
    Ok(best)
}
