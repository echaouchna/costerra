# Cloud IDS is not part of the synced price catalog yet ("Cloud IDS" must be
# added to SERVICES in src/pricing/sync.rs); kept apart from the main fixture.
provider "google" {
  project = "example-dev"
  region  = "europe-west1"
}

resource "google_cloud_ids_endpoint" "dev" {
  name     = "ids-dev"
  location = "europe-west1-b"
  network  = "projects/example-dev/global/networks/dev"
  severity = "INFORMATIONAL"
}
