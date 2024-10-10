use anyhow::{Context, Result};
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::{github, AppState};

pub type Scheduler = JobScheduler;

pub async fn create(state: AppState) -> Result<Scheduler> {
    let sched = JobScheduler::new().await?;
    sched
        .add(Job::new_async("0 0/5 * * * *", move |_uuid, _l| {
            let mut state = state.clone();
            Box::pin(async move {
                refresh_projects(&mut state).await.expect("Failed to refresh projects");
            })
        })?)
        .await?;
    sched.start().await?;
    Ok(sched)
}

pub async fn refresh_projects(state: &mut AppState) -> Result<()> {
    for project_info in state.db.get_projects().await? {
        github::run(state, &project_info.project.owner, &project_info.project.repo, 0)
            .await
            .with_context(|| {
                format!(
                    "Failed to refresh {}/{}",
                    project_info.project.owner, project_info.project.repo
                )
            })?;
    }
    Ok(())
}

// github::run(&mut state, "zeldaret", "tww", 9983071101)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "DarkRTA", "rb3", 10482280646)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "bfbbdecomp", "bfbb", 9726343619)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "projectPiki", "pikmin2", 10429582042)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "doldecomp", "melee", 10661324447)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "xbret", "xenoblade", 10687397293)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "SeekyCt", "spm-decomp", 10605206350)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "PrimeDecomp", "prime", 10713664775)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "PrimeDecomp", "echoes", 10717066271)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "Rainchus", "marioparty4", 10715620655)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "zeldaret", "ss", 10838420987)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "zeldaret", "oot-gc", 10964802446)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "projectPiki", "pikmin", 11066117942)
//     .await
//     .expect("Failed to run GitHub client");
// github::run(&mut state, "NSMBW-Community","NSMBW-Decomp", 11094989546)
//     .await
//     .expect("Failed to run GitHub client");
