locals {
  env_vars = read_terragrunt_config(find_in_parent_folders("env.hcl"))
  env      = local.env_vars.locals.env
  region   = local.env_vars.locals.region
  project  = "example-${local.env}"
}

generate "provider" {
  path      = "provider.tf"
  if_exists = "overwrite_terragrunt"
  contents  = <<EOT
provider "google" {
  project = "${local.project}"
  region  = "${local.region}"
}
EOT
}

remote_state {
  backend = "gcs"
  config = {
    bucket = "example-tfstate"
    prefix = path_relative_to_include()
  }
}

inputs = {
  project = local.project
  env     = local.env
  labels  = { env = local.env, managed_by = "terragrunt" }
}
