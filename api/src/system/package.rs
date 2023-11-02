use std::collections::{HashMap, HashSet};

use packageurl::PackageUrl;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, FromQueryResult,
    ModelTrait, QueryFilter, QuerySelect, Set, Statement,
};
use sea_query::Value;

use huevos_entity::package::{PackageNamespace, PackageType};
use huevos_entity::package_dependency::ToDependency;
use huevos_entity::{package, package_dependency, package_qualifier};

use crate::system::System;
use crate::{PackageTree, Purl};

impl System {
    pub async fn ingest_package<'p, P: Into<Purl>>(
        &self,
        pkg: P,
    ) -> Result<package::Model, anyhow::Error> {
        let purl = pkg.into();

        let pkg = self
            .insert_or_fetch_package(
                &purl.ty,
                purl.namespace.as_deref(),
                &purl.name,
                &purl.version,
                &purl.qualifiers,
            )
            .await?;

        Ok(pkg)
    }

    pub async fn fetch_package<'p, P: Into<Purl>>(
        &self,
        pkg: P,
    ) -> Result<Option<package::Model>, anyhow::Error> {
        let purl = pkg.into();
        self.get_package(
            &purl.ty,
            &purl.namespace.as_deref(),
            &purl.name,
            &purl.version,
            &purl.qualifiers,
        )
        .await
    }

    pub async fn packages(&self) -> Result<Vec<Purl>, anyhow::Error> {
        let found = package::Entity::find()
            .find_with_related(package_qualifier::Entity)
            .all(&*self.db)
            .await?;

        println!("{:#?}", found);

        Ok(self.packages_to_purls(found)?)
    }

    pub async fn insert_or_fetch_package<'a>(
        &self,
        r#type: &str,
        namespace: Option<&str>,
        name: &str,
        version: &str,
        qualifiers: &HashMap<String, String>,
    ) -> Result<package::Model, anyhow::Error> {
        let fetch = self
            .get_package(r#type, &namespace, name, version, qualifiers)
            .await?;
        if let Some(pkg) = fetch {
            Ok(pkg)
        } else {
            let mut entity = package::ActiveModel {
                package_type: Set(r#type.to_string()),
                package_namespace: Default::default(),
                package_name: Set(name.to_string()),
                version: Set(version.to_owned()),
                ..Default::default()
            };

            if let Some(ns) = namespace {
                entity.package_namespace = Set(Some(ns.to_string()))
            }

            let inserted = entity.insert(&*self.db).await?;

            for (k, v) in qualifiers {
                let entity = package_qualifier::ActiveModel {
                    package_id: Set(inserted.id),
                    key: Set(k.to_string()),
                    value: Set(v.to_string()),
                    ..Default::default()
                };
                entity.insert(&*self.db).await?;
            }

            Ok(inserted)
        }
    }

    async fn get_package<'a>(
        &self,
        r#type: &str,
        namespace: &Option<&str>,
        name: &str,
        version: &str,
        qualifiers: &HashMap<String, String>,
    ) -> Result<Option<package::Model>, anyhow::Error> {
        let mut conditions = Condition::all()
            .add(package::Column::PackageType.eq(r#type.to_string()))
            .add(package::Column::PackageName.eq(name.to_string()))
            .add(package::Column::Version.eq(version));

        if let Some(ns) = namespace {
            conditions = conditions.add(package::Column::PackageNamespace.eq(ns.to_string()));
        } else {
            conditions = conditions.add(package::Column::PackageNamespace.is_null());
        }

        let found = package::Entity::find()
            .find_with_related(package_qualifier::Entity)
            .filter(conditions)
            .all(&*self.db)
            .await?;

        if found.is_empty() {
            return Ok(None);
        } else {
            for (found_package, found_qualifiers) in found {
                if qualifiers.is_empty() && found_qualifiers.is_empty() {
                    return Ok(Some(found_package));
                }

                if qualifiers.len() != found_qualifiers.len() {
                    return Ok(None);
                }

                for (expected_k, expected_v) in qualifiers {
                    if found_qualifiers
                        .iter()
                        .any(|found_q| found_q.key == *expected_k && found_q.value == *expected_v)
                    {
                        return Ok(Some(found_package));
                    }
                }
            }
        }

        Ok(None)
    }

    pub async fn package_types(&self) -> Result<Vec<String>, anyhow::Error> {
        Ok(package::Entity::find()
            .select_only()
            .column(package::Column::PackageType)
            .group_by(package::Column::PackageType)
            .into_model::<PackageType>()
            .all(&*self.db)
            .await?
            .iter()
            .map(|e| e.package_type.clone())
            .collect())
    }

    pub async fn package_namespaces(&self) -> Result<Vec<String>, anyhow::Error> {
        Ok(package::Entity::find()
            .select_only()
            .column(package::Column::PackageNamespace)
            .group_by(package::Column::PackageNamespace)
            .into_model::<PackageNamespace>()
            .all(&*self.db)
            .await?
            .iter()
            .map(|e| e.package_namespace.clone())
            .collect())
    }

    // ------------------------------------------------------------------------
    // ------------------------------------------------------------------------

    pub async fn ingest_package_dependency<P1: Into<Purl>, P2: Into<Purl>>(
        &self,
        dependent_package: P1,
        dependency_package: P2,
    ) -> Result<package_dependency::Model, anyhow::Error> {
        let dependent = self.ingest_package(dependent_package).await?;
        let dependency = self.ingest_package(dependency_package).await?;

        match package_dependency::Entity::find()
            .filter(
                Condition::all()
                    .add(package_dependency::Column::DependentPackageId.eq(dependent.id))
                    .add(package_dependency::Column::DependencyPackageId.eq(dependency.id)),
            )
            .one(&*self.db)
            .await?
        {
            None => {
                let entity = package_dependency::ActiveModel {
                    dependent_package_id: Set(dependent.id),
                    dependency_package_id: Set(dependency.id),
                };

                Ok(entity.insert(&*self.db).await?)
            }
            Some(found) => Ok(found),
        }
    }

    fn packages_to_purls(
        &self,
        packages: Vec<(package::Model, Vec<package_qualifier::Model>)>,
    ) -> Result<Vec<Purl>, anyhow::Error> {
        let mut purls = Vec::new();

        for (base, qualifiers) in &packages {
            purls.push(self.package_to_purl(base.clone(), qualifiers.clone())?);
        }

        Ok(purls)
    }

    fn package_to_purl(
        &self,
        base: package::Model,
        qualifiers: Vec<package_qualifier::Model>,
    ) -> Result<Purl, anyhow::Error> {
        let mut purl = PackageUrl::new(base.package_type.clone(), base.package_name.clone())?;

        purl.with_version(base.version.clone());

        if let Some(namespace) = &base.package_namespace {
            purl.with_namespace(namespace.clone());
        }

        for qualifier in qualifiers {
            purl.add_qualifier(qualifier.key.clone(), qualifier.value.clone())?;
        }

        Ok(purl.into())
    }

    pub async fn direct_dependencies<'p, P: Into<Purl>>(
        &self,
        dependent_package: P,
    ) -> Result<Vec<Purl>, anyhow::Error> {
        let dependent = self.ingest_package(dependent_package).await?;

        let found = dependent
            .find_linked(ToDependency)
            .find_with_related(package_qualifier::Entity)
            .all(&*self.db)
            .await?;

        Ok(self.packages_to_purls(found)?)
    }

    /*
    pub async fn transitive_dependencies<'p, P: Into<Purl<'p>>>(
        &'p self,
        root: P,
    ) -> Result<PackageTree<'p>, anyhow::Error> {
        let root_purl = root.into();

        let mut purls = HashMap::new();
        let mut queue = Vec::new();
        queue.push(root_purl.clone());

        while let Some(cur) = queue.pop() {
            let dependencies = self.direct_dependencies(cur.clone()).await?;
            queue.extend_from_slice(&dependencies);
            purls.insert(cur, dependencies);
        }

        fn build_tree<'p>(
            root: &Purl<'p>,
            map: &HashMap<Purl<'p>, Vec<Purl<'p>>>,
        ) -> PackageTree<'p> {
            let dependencies = map
                .get(root)
                .iter()
                .flat_map(|deps| deps.iter().map(|dep| build_tree(dep, map)))
                .collect();

            PackageTree {
                purl: root.clone(),
                dependencies,
            }
        }

        Ok(build_tree(&root_purl, &purls))

    }
     */

    pub async fn transitive_dependencies<P: Into<Purl>>(
        &self,
        root: P,
    ) -> Result<PackageTree, anyhow::Error> {
        let root_model = self.ingest_package(root).await?;
        let root_id = Value::Int(Some(root_model.id));

        let relationships = package_dependency::Entity::find()
            .from_raw_sql(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                r#"
                    WITH RECURSIVE transitive AS (
                        SELECT
                            timestamp, dependent_package_id, dependency_package_id
                        FROM
                            package_dependency
                        WHERE
                            dependent_package_id = $1
                        UNION
                        SELECT
                            pd.timestamp, pd.dependent_package_id, pd.dependency_package_id
                        FROM
                            package_dependency pd
                        INNER JOIN transitive transitive1
                            ON pd.dependent_package_id = transitive1.dependency_package_id
                    )
                    SELECT * FROM transitive
                    "#,
                vec![root_id],
            ))
            .all(&*self.db)
            .await?;

        let mut dependencies = HashMap::new();
        let mut all_packages = HashSet::new();

        for relationship in relationships {
            all_packages.insert(relationship.dependent_package_id);
            all_packages.insert(relationship.dependency_package_id);
            dependencies
                .entry(relationship.dependent_package_id)
                .or_insert(Vec::new())
                .push(relationship.dependency_package_id)
        }

        let mut purls = HashMap::new();

        for pkg_id in all_packages {
            let pkg = package::Entity::find_by_id(pkg_id)
                .find_with_related(package_qualifier::Entity)
                .all(&*self.db)
                .await?;

            if !pkg.is_empty() {
                let (base, qualifiers) = &pkg[0];
                let purl = self.package_to_purl(base.clone(), qualifiers.clone())?;
                purls.insert(pkg_id, purl);
            }
        }

        fn build_tree(
            root: i32,
            relationships: &HashMap<i32, Vec<i32>>,
            purls: &HashMap<i32, Purl>,
        ) -> PackageTree {
            let dependencies = relationships
                .get(&root)
                .iter()
                .flat_map(|deps| {
                    deps.iter()
                        .map(|dep| build_tree(*dep, relationships, purls))
                })
                .collect();

            PackageTree {
                id: root,
                purl: purls[&root].clone(),
                dependencies,
            }
        }

        Ok(build_tree(root_model.id, &dependencies, &purls))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::system::System;
    use crate::Purl;

    #[tokio::test]
    async fn ingest_packages() -> Result<(), anyhow::Error> {
        let system = System::for_test("ingest_packages").await?;

        let mut packages = vec![
            "pkg:maven/io.quarkus/quarkus-hibernate-orm@2.13.5.Final?type=jar",
            "pkg:maven/io.quarkus/quarkus-core@2.13.5.Final?type=jar",
            "pkg:maven/jakarta.el/jakarta.el-api@3.0.3?type=jar",
            "pkg:maven/org.postgresql/postgresql@42.5.0?type=jar",
            "pkg:maven/io.quarkus/quarkus-narayana-jta@2.13.5.Final?type=jar",
            "pkg:maven/jakarta.interceptor/jakarta.interceptor-api@1.2.5?type=jar",
            "pkg:maven/com.fasterxml.jackson.core/jackson-databind@2.13.1?type=jar",
            "pkg:maven/io.quarkus/quarkus-jdbc-postgresql@2.13.5.Final?type=jar",
            "pkg:maven/jakarta.enterprise/jakarta.enterprise.cdi-api@2.0.2?type=jar",
            "pkg:maven/jakarta.enterprise/jakarta.enterprise.cdi-api@2.0.2?type=jar",
            "pkg:maven/jakarta.enterprise/jakarta.enterprise.cdi-api@2.0.2?type=war",
            "pkg:maven/jakarta.enterprise/jakarta.enterprise.cdi-api@2.0.2?type=jar&cheese=cheddar",
            "pkg:maven/org.apache.logging.log4j/log4j-core@2.13.3",
        ];

        for pkg in &packages {
            system.ingest_package(pkg).await?;
        }

        let package_types = system.package_types().await?;

        let package_namespaces = system.package_namespaces().await?;

        let fetched_packages = system.packages().await?;

        let packages: HashSet<_> = packages.drain(..).collect();

        assert_eq!(fetched_packages.len(), packages.len());

        //for pkg in fetched_packages {
        //println!("{}", pkg.to_string());
        //}

        Ok(())
    }

    #[tokio::test]
    async fn ingest_package_dependencies() -> Result<(), anyhow::Error> {
        let system = System::for_test("ingest_package_dependencies").await?;

        let result = system
            .ingest_package_dependency(
                "pkg:maven/io.quarkus/quarkus-jdbc-postgresql@2.13.5.Final?type=jar",
                "pkg:maven/io.quarkus/quarkus-jdbc-base@1.13.5.Final?type=jar",
            )
            .await?;

        let result = system
            .ingest_package_dependency(
                "pkg:maven/io.quarkus/quarkus-jdbc-postgresql@2.13.5.Final?type=jar",
                "pkg:maven/io.quarkus/quarkus-postgres@1.13.5.Final?type=jar",
            )
            .await?;

        let result = system
            .direct_dependencies(
                "pkg:maven/io.quarkus/quarkus-jdbc-postgresql@2.13.5.Final?type=jar",
            )
            .await?;

        println!("{:?}", result);
        Ok(())
    }

    #[tokio::test]
    async fn transitive_dependencies() -> Result<(), anyhow::Error> {
        let system = System::for_test("transitive_dependencies").await?;

        system
            .ingest_package_dependency(
                "pkg:maven/com.test/package-a@1.0?type=jar",
                "pkg:maven/com.test/package-ab@1.0?type=jar",
            )
            .await?;

        system
            .ingest_package_dependency(
                "pkg:maven/com.test/package-a@1.0?type=jar",
                "pkg:maven/com.test/package-ac@1.0?type=jar",
            )
            .await?;

        system
            .ingest_package_dependency(
                "pkg:maven/com.test/package-ac@1.0?type=jar",
                "pkg:maven/com.test/package-acd@1.0?type=jar",
            )
            .await?;

        system
            .ingest_package_dependency(
                "pkg:maven/com.test/package-ab@1.0?type=jar",
                "pkg:maven/com.test/package-ac@1.0?type=jar",
            )
            .await?;

        let result = system
            .transitive_dependencies("pkg:maven/com.test/package-a@1.0?type=jar")
            .await?;

        assert_eq!(
            Purl::from("pkg:maven/com.test/package-a@1.0?type=jar"),
            result.purl
        );
        assert_eq!(2, result.dependencies.len());

        Ok(())
    }
}