Homeport server
===============

homeportd serves folders on this machine to the Homeport app on a Mac,
over Tailscale. homeport is a small command-line client for testing a
server.

Quick start (Tailscale must be running and logged in):

  homeportd share add <folder>     share a folder (repeatable)
  homeportd service install        run at boot / login and start now
  homeportd pair                   show the link and code for the Mac app
  homeportd service status         check it is running

Installers that do all of this, and full documentation:

  https://github.com/R44VC0RP/homeport#install

Licensed under the Apache License 2.0 (see LICENSE).
