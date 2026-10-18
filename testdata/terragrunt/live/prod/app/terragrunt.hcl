include "root" {
  path = find_in_parent_folders("root.hcl")
}

terraform {
  source = "../../../modules//app"
}

dependency "network" {
  config_path = "../network"
}

inputs = {
  subnet       = dependency.network.outputs.subnet
  machine_type = "n2-standard-8"
  replicas     = 4
}
