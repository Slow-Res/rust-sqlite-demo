# GET /api/health: liveness plus a database round-trip.
class HealthController < ApplicationController
  def show
    ApplicationRecord.lease_connection.select_value("SELECT 1")
    render_health("ok", :ok)
  rescue StandardError => e
    Rails.logger.error("health check failed: #{e.class}: #{e.message}")
    render_health("unavailable", :service_unavailable)
  end

  private

  def render_health(status, http_status)
    render json: { status: status, node_id: Settings::NODE_ID, version: LibraryRails::VERSION }, status: http_status
  end
end
